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
use std::sync::Arc;

use bamts_compiler::diagnostic::{Diagnostic, DiagnosticSeverity};
use bamts_compiler::source::{ScriptKind, SourceId, SourceText};

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
    /// The source file could not be read.
    ReadSource {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The source file extension does not select a supported syntax.
    UnsupportedSourceExtension { path: PathBuf },
    /// Parsing produced one or more error diagnostics.
    Diagnostics {
        path: PathBuf,
        diagnostics: Vec<Diagnostic>,
    },
    /// The parsed source cannot be represented in verified BamTS bytecode.
    Lower(bamts_compiler::lower::LowerError),
    /// The verified bytecode did not execute successfully.
    Runtime(bamts_runtime::RuntimeError),
    /// Native object emission failed.
    #[cfg(feature = "aot")]
    Aot(bamts_codegen::AotError),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadSource { path, source } => {
                write!(
                    formatter,
                    "could not read source file `{}`: {source}",
                    path.display()
                )
            }
            Self::UnsupportedSourceExtension { path } => write!(
                formatter,
                "source file `{}` has an unsupported extension",
                path.display()
            ),
            Self::Diagnostics { path, diagnostics } => write!(
                formatter,
                "source file `{}` has {} error diagnostic(s)",
                path.display(),
                diagnostics.len()
            ),
            Self::Lower(error) => write!(formatter, "could not compile source: {error}"),
            Self::Runtime(error) => write!(formatter, "program execution failed: {error}"),
            #[cfg(feature = "aot")]
            Self::Aot(error) => write!(formatter, "could not emit native object: {error}"),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::ReadSource { source, .. } => Some(source),
            Self::UnsupportedSourceExtension { .. } | Self::Diagnostics { .. } => None,
            Self::Lower(error) => Some(error),
            Self::Runtime(error) => Some(error),
            #[cfg(feature = "aot")]
            Self::Aot(error) => Some(error),
        }
    }
}

impl From<bamts_compiler::lower::LowerError> for Error {
    fn from(error: bamts_compiler::lower::LowerError) -> Self {
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

/// Compiles one source file into verified BamTS bytecode.
///
/// ```
/// # fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
/// let path = std::env::temp_dir().join(format!(
///     "bamts-facade-compile-{}.ts",
///     std::process::id(),
/// ));
/// std::fs::write(&path, "let answer = 42;")?;
/// let bytecode = bamts::compile_source_file(&path)?;
/// std::fs::remove_file(path)?;
/// assert_eq!(bytecode.functions().len(), 1);
/// # Ok(())
/// # }
/// ```
pub fn compile_source_file(
    path: impl AsRef<Path>,
) -> Result<bamts_bytecode::Module<bamts_bytecode::Verified>> {
    let path = path.as_ref();
    let source = fs::read_to_string(path).map_err(|source| Error::ReadSource {
        path: path.to_owned(),
        source,
    })?;
    let script_kind = script_kind(path)?;
    let scanned = bamts_compiler::scanner::scan(
        SourceId::new(0),
        script_kind,
        Arc::new(SourceText::new(source)),
    );
    let parsed = bamts_compiler::parser::parse(scanned);
    let diagnostics = parsed.diagnostics();
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity() == DiagnosticSeverity::Error)
    {
        return Err(Error::Diagnostics {
            path: path.to_owned(),
            diagnostics: diagnostics.to_vec(),
        });
    }

    bamts_compiler::lower::lower(
        parsed.product(),
        bamts_compiler::lower::LowerOptions {
            javascript_compatibility: matches!(
                script_kind,
                ScriptKind::JavaScript | ScriptKind::JavaScriptReact
            ),
        },
    )
    .map_err(Error::from)
}

/// Runs one source file with the deterministic Node-compatible host.
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
    let bytecode = compile_source_file(path)?;
    let mut host = bamts_node::NodeHost::new();
    bamts_runtime::run(&bytecode, &mut host, &bamts_runtime::Limits::default())?;

    Ok(ProgramOutput {
        stdout: host.stdout().to_vec(),
        exit_code: host.exit_code(),
    })
}

/// Compiles one source file into a relocatable native object for `target`.
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
    let bytecode = compile_source_file(path)?;
    bamts_codegen::compile_aot(&bytecode, target).map_err(Error::Aot)
}

#[cfg(feature = "aot")]
impl From<bamts_codegen::AotError> for Error {
    fn from(error: bamts_codegen::AotError) -> Self {
        Self::Aot(error)
    }
}

fn script_kind(path: &Path) -> Result<ScriptKind> {
    let extension = path.extension().and_then(|extension| extension.to_str());
    match extension {
        Some("js" | "mjs" | "cjs") => Ok(ScriptKind::JavaScript),
        Some("jsx") => Ok(ScriptKind::JavaScriptReact),
        Some("ts" | "mts" | "cts") => Ok(ScriptKind::TypeScript),
        Some("tsx") => Ok(ScriptKind::TypeScriptReact),
        _ => Err(Error::UnsupportedSourceExtension {
            path: path.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::compile_source_file;
    #[cfg(feature = "node-host")]
    use super::run_program;
    use std::error::Error;
    use std::path::PathBuf;

    fn fixture_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("bamts-facade-{name}-{}.ts", std::process::id()))
    }

    #[test]
    fn compiles_source_to_verified_bytecode() -> Result<(), Box<dyn Error>> {
        let path = fixture_path("compile");
        std::fs::write(&path, "let answer = 42;")?;
        let bytecode = compile_source_file(&path)?;
        std::fs::remove_file(path)?;

        assert_eq!(bytecode.functions().len(), 1);
        Ok(())
    }

    #[cfg(feature = "node-host")]
    #[test]
    fn runs_program_and_returns_host_output() -> Result<(), Box<dyn Error>> {
        let path = fixture_path("run");
        std::fs::write(&path, "console.log(42);")?;
        let output = run_program(&path)?;
        std::fs::remove_file(path)?;

        assert_eq!(output.stdout, b"42\n");
        assert_eq!(output.exit_code, 0);
        Ok(())
    }

    #[cfg(feature = "aot")]
    #[test]
    fn compiles_source_to_native_object() -> Result<(), Box<dyn Error>> {
        let path = fixture_path("aot");
        std::fs::write(&path, "let answer = 42;")?;
        let object = super::compile_native_object(&path, "x86_64")?;
        std::fs::remove_file(path)?;

        assert!(!object.bytes.is_empty());
        Ok(())
    }
}
