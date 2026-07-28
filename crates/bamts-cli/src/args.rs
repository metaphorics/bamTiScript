//! CLI Argument Parsing for `bamts`.
//!
//! Provides a deterministic, dependency-free argument parser supporting:
//! - Operational modes (`check`, `compile`, `run`)
//! - Execution targets (`Aot`, `Jit`)
//! - JS compatibility options
//! - Output file / directory configurations
//! - Diagnostics format configuration
//! - Help and version information
//! - Conflict detection and stable, typed usage errors
//! - Positional entrypoints and `--` program arguments

use std::fmt;

use bamts_compiler::lint::{LintLevel, rule_by_name};

/// The operational mode for the CLI compiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Mode {
    /// Type-check source files without emitting output artifacts.
    #[default]
    Check,
    /// Compile source files to executable or artifact outputs.
    Compile,
    /// Execute the compiled program.
    Run,
    /// Explain a registered lint rule.
    Explain,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Check => write!(f, "check"),
            Self::Compile => write!(f, "compile"),
            Self::Run => write!(f, "run"),
            Self::Explain => write!(f, "explain"),
        }
    }
}

/// The execution target for compilation or execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ExecutionTarget {
    /// Ahead-Of-Time compilation/execution target.
    #[default]
    Aot,
    /// Just-In-Time execution target.
    Jit,
}

impl fmt::Display for ExecutionTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Aot => write!(f, "aot"),
            Self::Jit => write!(f, "jit"),
        }
    }
}

/// Diagnostic output formatting choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DiagnosticsFormat {
    /// Human-readable plain text output.
    #[default]
    Text,
    /// Rich formatted text output with code snippets.
    Pretty,
    /// JSON formatted diagnostic objects.
    Json,
    /// GitHub Actions workflow command annotations.
    Github,
    /// Compact single-line diagnostics (`file:line:col: level: message`).
    Compact,
}

impl fmt::Display for DiagnosticsFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text => write!(f, "text"),
            Self::Pretty => write!(f, "pretty"),
            Self::Json => write!(f, "json"),
            Self::Github => write!(f, "github"),
            Self::Compact => write!(f, "compact"),
        }
    }
}

/// JavaScript compatibility mode setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum JsCompatMode {
    /// Standard TypeScript/ECMAScript semantics.
    #[default]
    Standard,
    /// Latest ECMAScript draft features (ESNext).
    EsNext,
    /// ECMAScript 2022 semantics.
    Es2022,
    /// Node.js compatibility semantics.
    Node,
    /// Strict JavaScript checks.
    Strict,
    /// Loose/permissive JavaScript handling.
    Loose,
}

impl fmt::Display for JsCompatMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Standard => write!(f, "standard"),
            Self::EsNext => write!(f, "esnext"),
            Self::Es2022 => write!(f, "es2022"),
            Self::Node => write!(f, "node"),
            Self::Strict => write!(f, "strict"),
            Self::Loose => write!(f, "loose"),
        }
    }
}

/// Options controlling JavaScript compatibility behavior.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JsCompatOptions {
    /// Whether JS compatibility options are active.
    pub enabled: bool,
    /// Specific compatibility mode.
    pub mode: JsCompatMode,
    /// Allow parsing `.js` and `.jsx` input files.
    pub allow_js: bool,
    /// Type-check JavaScript source files.
    pub check_js: bool,
    /// Preserve JSX constructs in emitted output.
    pub jsx_preserve: bool,
}

/// Options controlling output files and directories.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutputOptions {
    /// Explicit single output file path (`-o`, `--output`).
    pub file: Option<String>,
    /// Directory for emitted files (`--out-dir`, `--output-dir`).
    pub dir: Option<String>,
    /// Emit `.d.ts` declaration files (`-d`, `--emit-declarations`).
    pub emit_declarations: bool,
    /// Emit `.map` source map files (`-m`, `--source-maps`).
    pub source_maps: bool,
}

/// Structured CLI arguments parsed from the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliArgs {
    /// The operational mode.
    pub mode: Mode,
    /// The execution target (AOT vs JIT).
    pub target: ExecutionTarget,
    /// Positional primary entrypoint source file.
    pub entrypoint: Option<String>,
    /// Additional positional input files (if any).
    pub extra_inputs: Vec<String>,
    /// Arguments passed directly to the executed program after `--`.
    pub program_args: Vec<String>,
    /// JS compatibility configurations.
    pub js_compat: JsCompatOptions,
    /// Output destination and emit options.
    pub output: OutputOptions,
    /// Formatting style for diagnostic messages.
    pub diagnostics_format: DiagnosticsFormat,
    /// Ordered command-line lint-level overrides.
    pub lint_overrides: Vec<LintOverrideArg>,
    /// Enables the strict lint profile before project configuration is applied.
    pub strict: bool,
    /// Enables the pedantic lint profile before project configuration is applied.
    pub pedantic: bool,
    /// The maximum number of diagnostics rendered by one command.
    pub error_limit: usize,
    /// Rule requested by the `explain` subcommand.
    pub explain_rule: Option<String>,
    /// Print help information flag (`-h`, `--help`).
    pub help: bool,
    /// Print version information flag (`-V`, `--version`).
    pub version: bool,
}

/// One order-sensitive lint-level override from the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintOverrideArg {
    /// The exact rule code, rule slug, or group name selected by the user.
    pub selector: String,
    /// The level assigned to the selector.
    pub level: LintLevel,
}

impl LintOverrideArg {
    #[must_use]
    pub fn new(selector: impl Into<String>, level: LintLevel) -> Self {
        Self {
            selector: selector.into(),
            level,
        }
    }
}

impl CliArgs {
    /// Returns `true` if mode is [`Mode::Check`].
    pub fn is_check(&self) -> bool {
        self.mode == Mode::Check
    }

    /// Returns `true` if mode is [`Mode::Compile`].
    pub fn is_compile(&self) -> bool {
        self.mode == Mode::Compile
    }

    /// Returns `true` if mode is [`Mode::Run`].
    pub fn is_run(&self) -> bool {
        self.mode == Mode::Run
    }

    /// Returns `true` if the `explain` subcommand was selected.
    pub fn is_explain(&self) -> bool {
        self.mode == Mode::Explain
    }
}

/// Typed CLI usage error variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgsError {
    /// Unrecognized command-line option flag.
    UnknownOption { option: String },
    /// Option expected a value parameter, but none was provided.
    MissingOptionValue { option: String },
    /// Conflicting CLI operational modes specified.
    ConflictingModes { mode1: Mode, mode2: Mode },
    /// Conflicting execution targets specified.
    ConflictingTargets {
        target1: ExecutionTarget,
        target2: ExecutionTarget,
    },
    /// Incompatible command-line options specified together.
    ConflictingOptions { option1: String, option2: String },
    /// Option is incompatible with the selected mode.
    IncompatibleOption {
        option: String,
        mode: Mode,
        reason: String,
    },
    /// Invalid value supplied for an option.
    InvalidOptionValue {
        option: String,
        value: String,
        expected: String,
    },
    /// Required entrypoint positional argument missing for mode.
    MissingEntrypoint { mode: Mode },
    /// Multiple entrypoints specified where only one was expected.
    MultipleEntrypoints { first: String, second: String },
    /// Unexpected positional argument.
    UnexpectedArgument { arg: String },
    /// The explain subcommand was not given a rule name.
    MissingExplainRule,
    /// A lint's `forbid` level was lowered by a later setting.
    ForbiddenLintOverride {
        rule: String,
        forbidden_by: String,
        lowered_by: String,
    },
}

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption { option } => write!(f, "unknown option '{option}'"),
            Self::MissingOptionValue { option } => write!(f, "option '{option}' requires a value"),
            Self::ConflictingModes { mode1, mode2 } => {
                write!(
                    f,
                    "conflicting modes: cannot specify both '{mode1}' and '{mode2}'"
                )
            }
            Self::ConflictingTargets { target1, target2 } => {
                write!(
                    f,
                    "conflicting execution targets: cannot specify both '{target1}' and '{target2}'"
                )
            }
            Self::ConflictingOptions { option1, option2 } => {
                write!(
                    f,
                    "conflicting options: cannot specify both '{option1}' and '{option2}'"
                )
            }
            Self::IncompatibleOption {
                option,
                mode,
                reason,
            } => {
                write!(
                    f,
                    "option '{option}' is incompatible with {mode} mode: {reason}"
                )
            }
            Self::InvalidOptionValue {
                option,
                value,
                expected,
            } => {
                write!(
                    f,
                    "invalid value '{value}' for option '{option}'; expected one of: {expected}"
                )
            }
            Self::MissingEntrypoint { mode } => {
                write!(f, "missing required entrypoint file for {mode} mode")
            }
            Self::MultipleEntrypoints { first, second } => {
                write!(
                    f,
                    "multiple entrypoints specified ('{first}' and '{second}'); expected only one entrypoint"
                )
            }
            Self::UnexpectedArgument { arg } => write!(f, "unexpected argument '{arg}'"),
            Self::MissingExplainRule => write!(f, "missing rule name for explain subcommand"),
            Self::ForbiddenLintOverride {
                rule,
                forbidden_by,
                lowered_by,
            } => write!(
                f,
                "rule '{rule}' was forbidden by '{forbidden_by}' and cannot be lowered by '{lowered_by}'"
            ),
        }
    }
}

impl std::error::Error for ArgsError {}

/// Returns formatted CLI help documentation text.
pub fn help_message() -> &'static str {
    r#"bamts - TypeScript compiler and runtime for Rust

USAGE:
    bamts [SUBCOMMAND] [OPTIONS] [ENTRYPOINT] [-- PROGRAM_ARGS...]

SUBCOMMANDS:
    check       Type-check source files without emitting output artifacts
    compile     Compile TypeScript source files to output artifacts
    run         Execute TypeScript/JavaScript program
    explain     Explain a lint rule and its sound alternative

OPTIONS:
    -c, --compile               Select compile mode
    -r, --run                   Select run mode
        --check                 Select check mode

        --target <aot|jit>      Set execution target (aot or jit)
        --aot                   Alias for --target aot
        --jit                   Alias for --target jit

        --js-compat             Enable JavaScript compatibility options
        --compat <MODE>         Set JS compatibility mode (standard, esnext, es2022, node, strict, loose)
        --allow-js              Allow parsing .js and .jsx files
        --check-js              Enable type checking on JavaScript files
        --jsx-preserve          Preserve JSX constructs in emitted code

    -o, --output <FILE>         Specify output file path
        --out-dir <DIR>         Specify output directory
    -d, --emit-declarations     Emit declaration files (.d.ts)
    -m, --source-maps           Generate source maps (.map)

        --diagnostics-format <FMT>  Format for diagnostics (text, pretty, json, github, compact)
        --format <FMT>          Alias for --diagnostics-format
        --json                  Alias for --diagnostics-format json
        --pretty                Alias for --diagnostics-format pretty

    -A, --allow <RULE>          Set a rule or group to allow
    -W, --warn <RULE>           Set a rule or group to warn
    -D, --deny <RULE>           Set a rule or group to deny
    -F, --forbid <RULE>         Set a rule or group to forbid
        --strict                Enable strict lint profile
        --pedantic              Enable pedantic lint profile
        --error-limit <N>       Render at most N diagnostics (default: 50)

    -h, --help                  Print help information
    -V, --version               Print version information
"#
}

/// Returns version string formatted as `bamts <version>`.
pub fn version_message() -> &'static str {
    concat!("bamts ", env!("CARGO_PKG_VERSION"))
}

/// Formats the compiler-owned explanation for a stable code or rule slug.
pub fn explain_rule(name: &str) -> Result<String, ArgsError> {
    let rule = rule_by_name(name).ok_or_else(|| ArgsError::InvalidOptionValue {
        option: "explain".to_string(),
        value: name.to_string(),
        expected: "a stable BAMTS-W code or rule slug".to_string(),
    })?;
    Ok(format!(
        "{} ({})\nrationale: {}\nsound alternative: {}\nsilence: {}\n",
        rule.code(),
        rule.slug(),
        rule.rationale(),
        rule.sound_alternative(),
        rule.silence_flag(),
    ))
}

/// Parse command-line arguments from environment [`std::env::args`].
pub fn parse_env_args() -> Result<CliArgs, ArgsError> {
    parse_args(std::env::args())
}

/// Parse command-line arguments from any string iterator.
///
/// If the first item matches the executable binary name (`bamts` or path ending in `bamts`),
/// it is automatically skipped.
pub fn parse_args<I, T>(raw_args: I) -> Result<CliArgs, ArgsError>
where
    I: IntoIterator<Item = T>,
    T: AsRef<str>,
{
    let args_vec: Vec<String> = raw_args
        .into_iter()
        .map(|s| s.as_ref().to_string())
        .collect();
    let mut args_slice = args_vec.as_slice();

    // Automatically strip binary path/name if present as item 0
    if let Some(first) = args_slice.first()
        && !first.starts_with('-')
        && first != "check"
        && first != "compile"
        && first != "run"
        && (first == "bamts"
            || first.ends_with("/bamts")
            || first.ends_with("\\bamts")
            || first.ends_with("bamts.exe"))
    {
        args_slice = &args_slice[1..];
    }

    let mut mode: Option<Mode> = None;
    let mut target: Option<ExecutionTarget> = None;
    let mut explicit_target = false;
    let mut entrypoint: Option<String> = None;
    let mut extra_inputs: Vec<String> = Vec::new();
    let mut program_args: Vec<String> = Vec::new();

    let mut js_compat = JsCompatOptions::default();
    let mut output = OutputOptions::default();
    let mut diagnostics_format = DiagnosticsFormat::default();
    let mut help = false;
    let mut version = false;
    let mut lint_overrides = Vec::new();
    let mut strict = false;
    let mut pedantic = false;
    let mut error_limit = 50;
    let mut explain_rule = None;

    let mut idx = 0;
    while idx < args_slice.len() {
        let arg = &args_slice[idx];
        idx += 1;

        // End of CLI options; remaining args belong to target program
        if arg == "--" {
            while idx < args_slice.len() {
                program_args.push(args_slice[idx].clone());
                idx += 1;
            }
            break;
        }

        if arg.starts_with('-') {
            let (flag, inline_val) = if let Some(eq_pos) = arg.find('=') {
                (&arg[..eq_pos], Some(&arg[eq_pos + 1..]))
            } else {
                (arg.as_str(), None)
            };

            let get_val = |opt: &str,
                           inline: Option<&str>,
                           current_idx: &mut usize,
                           slice: &[String]|
             -> Result<String, ArgsError> {
                if let Some(val) = inline {
                    if val.is_empty() {
                        Err(ArgsError::MissingOptionValue {
                            option: opt.to_string(),
                        })
                    } else {
                        Ok(val.to_string())
                    }
                } else if *current_idx < slice.len() && !slice[*current_idx].starts_with('-') {
                    let val = slice[*current_idx].clone();
                    *current_idx += 1;
                    Ok(val)
                } else {
                    Err(ArgsError::MissingOptionValue {
                        option: opt.to_string(),
                    })
                }
            };

            match flag {
                "-h" | "--help" => {
                    help = true;
                }
                "-V" | "--version" => {
                    version = true;
                }
                "--check" => {
                    set_mode(&mut mode, Mode::Check)?;
                }
                "-c" | "--compile" => {
                    set_mode(&mut mode, Mode::Compile)?;
                }
                "-r" | "--run" => {
                    set_mode(&mut mode, Mode::Run)?;
                }
                "--target" => {
                    let val = get_val(flag, inline_val, &mut idx, args_slice)?;
                    let parsed_target = match val.to_lowercase().as_str() {
                        "aot" => ExecutionTarget::Aot,
                        "jit" => ExecutionTarget::Jit,
                        _ => {
                            return Err(ArgsError::InvalidOptionValue {
                                option: flag.to_string(),
                                value: val,
                                expected: "aot, jit".to_string(),
                            });
                        }
                    };
                    set_target(&mut target, parsed_target)?;
                    explicit_target = true;
                }
                "--aot" => {
                    set_target(&mut target, ExecutionTarget::Aot)?;
                    explicit_target = true;
                }
                "--jit" => {
                    set_target(&mut target, ExecutionTarget::Jit)?;
                    explicit_target = true;
                }
                "--js-compat" => {
                    js_compat.enabled = true;
                }
                "--compat" | "--js-compat-mode" => {
                    let val = get_val("--compat", inline_val, &mut idx, args_slice)?;
                    let parsed_mode = match val.to_lowercase().as_str() {
                        "standard" => JsCompatMode::Standard,
                        "esnext" => JsCompatMode::EsNext,
                        "es2022" => JsCompatMode::Es2022,
                        "node" => JsCompatMode::Node,
                        "strict" => JsCompatMode::Strict,
                        "loose" => JsCompatMode::Loose,
                        _ => {
                            return Err(ArgsError::InvalidOptionValue {
                                option: "--compat".to_string(),
                                value: val,
                                expected: "standard, esnext, es2022, node, strict, loose"
                                    .to_string(),
                            });
                        }
                    };
                    js_compat.mode = parsed_mode;
                    js_compat.enabled = true;
                }
                "--allow-js" => {
                    js_compat.allow_js = true;
                    js_compat.enabled = true;
                }
                "--check-js" => {
                    js_compat.check_js = true;
                    js_compat.enabled = true;
                }
                "--jsx-preserve" => {
                    js_compat.jsx_preserve = true;
                    js_compat.enabled = true;
                }
                "-o" | "--output" => {
                    let val = get_val("--output", inline_val, &mut idx, args_slice)?;
                    if output.dir.is_some() {
                        return Err(ArgsError::ConflictingOptions {
                            option1: "--output".to_string(),
                            option2: "--out-dir".to_string(),
                        });
                    }
                    output.file = Some(val);
                }
                "--out-dir" | "--output-dir" => {
                    let val = get_val("--out-dir", inline_val, &mut idx, args_slice)?;
                    if output.file.is_some() {
                        return Err(ArgsError::ConflictingOptions {
                            option1: "--output".to_string(),
                            option2: "--out-dir".to_string(),
                        });
                    }
                    output.dir = Some(val);
                }
                "-d" | "--emit-declarations" | "--declaration" => {
                    output.emit_declarations = true;
                }
                "-m" | "--source-maps" | "--sourcemap" => {
                    output.source_maps = true;
                }
                "--diagnostics-format" | "--format" => {
                    let val = get_val("--diagnostics-format", inline_val, &mut idx, args_slice)?;
                    let parsed_fmt = match val.to_lowercase().as_str() {
                        "text" => DiagnosticsFormat::Text,
                        "pretty" => DiagnosticsFormat::Pretty,
                        "json" => DiagnosticsFormat::Json,
                        "github" => DiagnosticsFormat::Github,
                        "compact" => DiagnosticsFormat::Compact,
                        _ => {
                            return Err(ArgsError::InvalidOptionValue {
                                option: "--diagnostics-format".to_string(),
                                value: val,
                                expected: "text, pretty, json, github, compact".to_string(),
                            });
                        }
                    };
                    diagnostics_format = parsed_fmt;
                }
                "--json" | "--json-diagnostics" => {
                    diagnostics_format = DiagnosticsFormat::Json;
                }
                "--pretty" | "--pretty-diagnostics" => {
                    diagnostics_format = DiagnosticsFormat::Pretty;
                }
                "-A" | "--allow" => {
                    lint_overrides.push(LintOverrideArg::new(
                        get_val(flag, inline_val, &mut idx, args_slice)?,
                        LintLevel::Allow,
                    ));
                }
                "-W" | "--warn" => {
                    lint_overrides.push(LintOverrideArg::new(
                        get_val(flag, inline_val, &mut idx, args_slice)?,
                        LintLevel::Warn,
                    ));
                }
                "-D" | "--deny" => {
                    lint_overrides.push(LintOverrideArg::new(
                        get_val(flag, inline_val, &mut idx, args_slice)?,
                        LintLevel::Deny,
                    ));
                }
                "-F" | "--forbid" => {
                    lint_overrides.push(LintOverrideArg::new(
                        get_val(flag, inline_val, &mut idx, args_slice)?,
                        LintLevel::Forbid,
                    ));
                }
                "--strict" => strict = true,
                "--pedantic" => pedantic = true,
                "--error-limit" => {
                    let value = get_val(flag, inline_val, &mut idx, args_slice)?;
                    error_limit = value.parse::<usize>().ok().filter(|limit| *limit > 0).ok_or_else(|| {
                        ArgsError::InvalidOptionValue {
                            option: "--error-limit".to_string(),
                            value,
                            expected: "a positive integer".to_string(),
                        }
                    })?;
                }
                _ => {
                    return Err(ArgsError::UnknownOption {
                        option: arg.clone(),
                    });
                }
            }
        } else {
            // Positional subcommands or entrypoint inputs
            match arg.as_str() {
                "check" => set_mode(&mut mode, Mode::Check)?,
                "compile" => set_mode(&mut mode, Mode::Compile)?,
                "run" => set_mode(&mut mode, Mode::Run)?,
                "explain" => set_mode(&mut mode, Mode::Explain)?,
                _ => {
                    if mode == Some(Mode::Explain) {
                        if explain_rule.replace(arg.clone()).is_some() {
                            return Err(ArgsError::UnexpectedArgument { arg: arg.clone() });
                        }
                    } else if entrypoint.is_some() {
                        extra_inputs.push(arg.clone());
                    } else {
                        entrypoint = Some(arg.clone());
                    }
                }
            }
        }
    }

    // Default mode logic:
    // If explicit mode was provided, use it.
    // If positional entrypoint was provided without explicit mode, default to Compile.
    // If neither was provided, default to Check.
    let effective_mode = mode.unwrap_or(if entrypoint.is_some() {
        Mode::Compile
    } else {
        Mode::Check
    });

    if effective_mode == Mode::Explain && explain_rule.is_none() && !help && !version {
        return Err(ArgsError::MissingExplainRule);
    }

    // Run mode requires exactly one entrypoint regardless of where `run`
    // appears relative to positional inputs. Check after the effective mode is
    // resolved so the constraint is order-independent.
    if effective_mode == Mode::Run
        && let Some(first) = &entrypoint
        && let Some(second) = extra_inputs.first()
    {
        return Err(ArgsError::MultipleEntrypoints {
            first: first.clone(),
            second: second.clone(),
        });
    }

    let effective_target = target.unwrap_or(ExecutionTarget::Aot);

    // Validate mode constraints when help/version are not requested
    if !help && !version {
        if effective_mode == Mode::Check {
            if output.file.is_some() {
                return Err(ArgsError::IncompatibleOption {
                    option: "--output".to_string(),
                    mode: Mode::Check,
                    reason: "check mode does not produce output artifacts".to_string(),
                });
            }
            if output.dir.is_some() {
                return Err(ArgsError::IncompatibleOption {
                    option: "--out-dir".to_string(),
                    mode: Mode::Check,
                    reason: "check mode does not produce output artifacts".to_string(),
                });
            }
            if explicit_target {
                return Err(ArgsError::IncompatibleOption {
                    option: "--target".to_string(),
                    mode: Mode::Check,
                    reason: "check mode does not use an execution target".to_string(),
                });
            }
        }

        if effective_mode != Mode::Explain && entrypoint.is_none() {
            return Err(ArgsError::MissingEntrypoint {
                mode: effective_mode,
            });
        }
    }

    Ok(CliArgs {
        mode: effective_mode,
        target: effective_target,
        entrypoint,
        extra_inputs,
        program_args,
        js_compat,
        output,
        diagnostics_format,
        lint_overrides,
        strict,
        pedantic,
        error_limit,
        explain_rule,
        help,
        version,
    })
}

fn set_mode(current: &mut Option<Mode>, new_mode: Mode) -> Result<(), ArgsError> {
    if let Some(existing) = *current
        && existing != new_mode
    {
        return Err(ArgsError::ConflictingModes {
            mode1: existing,
            mode2: new_mode,
        });
    }
    *current = Some(new_mode);
    Ok(())
}

fn set_target(
    current: &mut Option<ExecutionTarget>,
    new_target: ExecutionTarget,
) -> Result<(), ArgsError> {
    if let Some(existing) = *current
        && existing != new_target
    {
        return Err(ArgsError::ConflictingTargets {
            target1: existing,
            target2: new_target,
        });
    }
    *current = Some(new_target);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subcommands() {
        let args = parse_args(["check", "main.ts"]).unwrap();
        assert_eq!(args.mode, Mode::Check);
        assert_eq!(args.entrypoint.as_deref(), Some("main.ts"));

        let args = parse_args(["compile", "main.ts"]).unwrap();
        assert_eq!(args.mode, Mode::Compile);
        assert_eq!(args.entrypoint.as_deref(), Some("main.ts"));

        let args = parse_args(["run", "main.ts"]).unwrap();
        assert_eq!(args.mode, Mode::Run);
        assert_eq!(args.entrypoint.as_deref(), Some("main.ts"));
    }

    #[test]
    fn test_mode_flags() {
        let args = parse_args(["--check", "app.ts"]).unwrap();
        assert_eq!(args.mode, Mode::Check);
        assert!(args.is_check());

        let args = parse_args(["-c", "app.ts"]).unwrap();
        assert_eq!(args.mode, Mode::Compile);
        assert!(args.is_compile());

        let args = parse_args(["-r", "app.ts"]).unwrap();
        assert_eq!(args.mode, Mode::Run);
        assert!(args.is_run());
    }

    #[test]
    fn test_default_mode_compile() {
        let args = parse_args(["index.ts"]).unwrap();
        assert_eq!(args.mode, Mode::Compile);
        assert_eq!(args.entrypoint.as_deref(), Some("index.ts"));
    }

    #[test]
    fn test_target_parsing() {
        let args = parse_args(["run", "main.ts", "--target", "jit"]).unwrap();
        assert_eq!(args.target, ExecutionTarget::Jit);

        let args = parse_args(["compile", "main.ts", "--aot"]).unwrap();
        assert_eq!(args.target, ExecutionTarget::Aot);

        let args = parse_args(["compile", "main.ts", "--jit"]).unwrap();
        assert_eq!(args.target, ExecutionTarget::Jit);

        let args = parse_args(["compile", "main.ts", "--target=aot"]).unwrap();
        assert_eq!(args.target, ExecutionTarget::Aot);
    }

    #[test]
    fn test_js_compat_options() {
        let args = parse_args([
            "compile",
            "index.js",
            "--js-compat",
            "--compat",
            "node",
            "--allow-js",
            "--check-js",
            "--jsx-preserve",
        ])
        .unwrap();

        assert!(args.js_compat.enabled);
        assert_eq!(args.js_compat.mode, JsCompatMode::Node);
        assert!(args.js_compat.allow_js);
        assert!(args.js_compat.check_js);
        assert!(args.js_compat.jsx_preserve);
    }

    #[test]
    fn test_output_options() {
        let args = parse_args(["compile", "main.ts", "-o", "out/main.js", "-d", "-m"]).unwrap();

        assert_eq!(args.output.file.as_deref(), Some("out/main.js"));
        assert!(args.output.emit_declarations);
        assert!(args.output.source_maps);

        let args = parse_args(["compile", "main.ts", "--out-dir", "dist"]).unwrap();
        assert_eq!(args.output.dir.as_deref(), Some("dist"));
    }

    #[test]
    fn test_diagnostics_formats() {
        let args = parse_args(["compile", "main.ts", "--format", "json"]).unwrap();
        assert_eq!(args.diagnostics_format, DiagnosticsFormat::Json);

        let args = parse_args(["compile", "main.ts", "--pretty"]).unwrap();
        assert_eq!(args.diagnostics_format, DiagnosticsFormat::Pretty);

        let args = parse_args(["compile", "main.ts", "--diagnostics-format=github"]).unwrap();
        assert_eq!(args.diagnostics_format, DiagnosticsFormat::Github);
    }

    #[test]
    fn test_program_args_passthrough() {
        let args = parse_args([
            "run",
            "server.ts",
            "--jit",
            "--",
            "--port",
            "8080",
            "-v",
            "arg2",
        ])
        .unwrap();

        assert_eq!(args.mode, Mode::Run);
        assert_eq!(args.target, ExecutionTarget::Jit);
        assert_eq!(args.entrypoint.as_deref(), Some("server.ts"));
        assert_eq!(args.program_args, vec!["--port", "8080", "-v", "arg2"]);
    }

    #[test]
    fn test_help_and_version() {
        let args = parse_args(["--help"]).unwrap();
        assert!(args.help);

        let args = parse_args(["check", "-h"]).unwrap();
        assert!(args.help);
        assert_eq!(args.mode, Mode::Check);

        let args = parse_args(["-V"]).unwrap();
        assert!(args.version);

        assert!(help_message().contains("bamts"));
        assert!(version_message().starts_with("bamts "));
    }

    #[test]
    fn test_conflicting_modes() {
        let err = parse_args(["check", "compile", "main.ts"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::ConflictingModes {
                mode1: Mode::Check,
                mode2: Mode::Compile
            }
        );

        let err = parse_args(["--check", "--run", "main.ts"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::ConflictingModes {
                mode1: Mode::Check,
                mode2: Mode::Run
            }
        );
    }

    #[test]
    fn test_conflicting_targets() {
        let err = parse_args(["compile", "main.ts", "--aot", "--jit"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::ConflictingTargets {
                target1: ExecutionTarget::Aot,
                target2: ExecutionTarget::Jit
            }
        );
    }

    #[test]
    fn test_conflicting_outputs() {
        let err =
            parse_args(["compile", "main.ts", "-o", "out.js", "--out-dir", "dist"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::ConflictingOptions {
                option1: "--output".to_string(),
                option2: "--out-dir".to_string()
            }
        );

        let err =
            parse_args(["compile", "main.ts", "--out-dir", "dist", "-o", "out.js"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::ConflictingOptions {
                option1: "--output".to_string(),
                option2: "--out-dir".to_string()
            }
        );
    }

    #[test]
    fn test_incompatible_options_check_mode() {
        let err = parse_args(["check", "main.ts", "-o", "out.js"]).unwrap_err();
        assert!(matches!(
            err,
            ArgsError::IncompatibleOption {
                mode: Mode::Check,
                ..
            }
        ));

        let err = parse_args(["check", "main.ts", "--aot"]).unwrap_err();
        assert!(matches!(
            err,
            ArgsError::IncompatibleOption {
                mode: Mode::Check,
                ..
            }
        ));
    }

    #[test]
    fn test_missing_entrypoint() {
        let err = parse_args(["run"]).unwrap_err();
        assert_eq!(err, ArgsError::MissingEntrypoint { mode: Mode::Run });

        let err = parse_args(["compile"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::MissingEntrypoint {
                mode: Mode::Compile
            }
        );

        let err = parse_args(["check"]).unwrap_err();
        assert_eq!(err, ArgsError::MissingEntrypoint { mode: Mode::Check });
    }

    #[test]
    fn test_multiple_entrypoints_in_run_mode() {
        let err = parse_args(["run", "main.ts", "second.ts"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::MultipleEntrypoints {
                first: "main.ts".to_string(),
                second: "second.ts".to_string()
            }
        );
    }

    #[test]
    fn test_unknown_option() {
        let err = parse_args(["compile", "main.ts", "--foo"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::UnknownOption {
                option: "--foo".to_string()
            }
        );
    }

    #[test]
    fn test_missing_option_value() {
        let err = parse_args(["compile", "main.ts", "-o"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::MissingOptionValue {
                option: "--output".to_string()
            }
        );

        let err = parse_args(["compile", "main.ts", "--target"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::MissingOptionValue {
                option: "--target".to_string()
            }
        );
    }

    #[test]
    fn test_invalid_option_value() {
        let err = parse_args(["compile", "main.ts", "--target", "invalid"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::InvalidOptionValue {
                option: "--target".to_string(),
                value: "invalid".to_string(),
                expected: "aot, jit".to_string()
            }
        );
    }

    #[test]
    fn test_binary_name_stripping() {
        let args = parse_args(["bamts", "check", "app.ts"]).unwrap();
        assert_eq!(args.mode, Mode::Check);
        assert_eq!(args.entrypoint.as_deref(), Some("app.ts"));

        let args = parse_args(["/usr/local/bin/bamts", "run", "app.ts"]).unwrap();
        assert_eq!(args.mode, Mode::Run);
        assert_eq!(args.entrypoint.as_deref(), Some("app.ts"));
    }

    #[test]
    fn test_run_mode_single_entrypoint_any_order() {
        // run a.ts b.ts — explicit run before two positionals (original order)
        let err = parse_args(["run", "a.ts", "b.ts"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::MultipleEntrypoints {
                first: "a.ts".to_string(),
                second: "b.ts".to_string()
            }
        );

        // a.ts run b.ts — run between two positionals
        let err = parse_args(["a.ts", "run", "b.ts"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::MultipleEntrypoints {
                first: "a.ts".to_string(),
                second: "b.ts".to_string()
            }
        );

        // a.ts b.ts run — run after two positionals
        let err = parse_args(["a.ts", "b.ts", "run"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::MultipleEntrypoints {
                first: "a.ts".to_string(),
                second: "b.ts".to_string()
            }
        );

        // run a.ts run b.ts — duplicate run flag before/after target
        let err = parse_args(["run", "a.ts", "run", "b.ts"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::MultipleEntrypoints {
                first: "a.ts".to_string(),
                second: "b.ts".to_string()
            }
        );

        // a.ts run b.ts run — duplicate run flag after target
        let err = parse_args(["a.ts", "run", "b.ts", "run"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::MultipleEntrypoints {
                first: "a.ts".to_string(),
                second: "b.ts".to_string()
            }
        );

        // --run a.ts b.ts — flag form before two positionals
        let err = parse_args(["--run", "a.ts", "b.ts"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::MultipleEntrypoints {
                first: "a.ts".to_string(),
                second: "b.ts".to_string()
            }
        );

        // -r a.ts b.ts — short flag form
        let err = parse_args(["-r", "a.ts", "b.ts"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::MultipleEntrypoints {
                first: "a.ts".to_string(),
                second: "b.ts".to_string()
            }
        );

        // Options interleaved: a.ts --jit run b.ts — extra input after options
        let err = parse_args(["a.ts", "--jit", "run", "b.ts"]).unwrap_err();
        assert_eq!(
            err,
            ArgsError::MultipleEntrypoints {
                first: "a.ts".to_string(),
                second: "b.ts".to_string()
            }
        );
    }

    #[test]
    fn test_run_mode_single_entrypoint_succeeds() {
        // Single entrypoint in run mode succeeds in any position.
        let args = parse_args(["run", "a.ts"]).unwrap();
        assert_eq!(args.mode, Mode::Run);
        assert_eq!(args.entrypoint.as_deref(), Some("a.ts"));
        assert!(args.extra_inputs.is_empty());

        let args = parse_args(["a.ts", "run"]).unwrap();
        assert_eq!(args.mode, Mode::Run);
        assert_eq!(args.entrypoint.as_deref(), Some("a.ts"));
        assert!(args.extra_inputs.is_empty());

        // -- passthrough preserves single entrypoint in run mode.
        let args = parse_args(["run", "a.ts", "--", "--port", "8080"]).unwrap();
        assert_eq!(args.mode, Mode::Run);
        assert_eq!(args.entrypoint.as_deref(), Some("a.ts"));
        assert_eq!(args.program_args, vec!["--port", "8080"]);
    }

    #[test]
    fn test_compile_check_multiple_inputs_preserved() {
        // compile and check modes keep multiple-input semantics in any order.
        let args = parse_args(["compile", "a.ts", "b.ts", "c.ts"]).unwrap();
        assert_eq!(args.mode, Mode::Compile);
        assert_eq!(args.entrypoint.as_deref(), Some("a.ts"));
        assert_eq!(args.extra_inputs, vec!["b.ts", "c.ts"]);

        let args = parse_args(["a.ts", "b.ts", "compile"]).unwrap();
        assert_eq!(args.mode, Mode::Compile);
        assert_eq!(args.entrypoint.as_deref(), Some("a.ts"));
        assert_eq!(args.extra_inputs, vec!["b.ts"]);

        let args = parse_args(["check", "a.ts", "b.ts"]).unwrap();
        assert_eq!(args.mode, Mode::Check);
        assert_eq!(args.entrypoint.as_deref(), Some("a.ts"));
        assert_eq!(args.extra_inputs, vec!["b.ts"]);

        let args = parse_args(["a.ts", "b.ts", "check"]).unwrap();
        assert_eq!(args.mode, Mode::Check);
        assert_eq!(args.entrypoint.as_deref(), Some("a.ts"));
        assert_eq!(args.extra_inputs, vec!["b.ts"]);
    }

    #[test]
    fn strictness_flags_preserve_selector_order() {
        let args = parse_args([
            "check",
            "-W",
            "escape-hatches",
            "-A",
            "BAMTS-W017",
            "-D",
            "explicit-any",
            "--strict",
            "--pedantic",
            "--error-limit=7",
            "main.ts",
        ])
        .expect("strictness flags parse");
        assert_eq!(
            args.lint_overrides,
            vec![
                LintOverrideArg::new("escape-hatches", LintLevel::Warn),
                LintOverrideArg::new("BAMTS-W017", LintLevel::Allow),
                LintOverrideArg::new("explicit-any", LintLevel::Deny),
            ]
        );
        assert!(args.strict);
        assert!(args.pedantic);
        assert_eq!(args.error_limit, 7);
    }

    #[test]
    fn explain_accepts_code_and_uses_catalog_metadata() {
        let args = parse_args(["explain", "BAMTS-W017"]).expect("explain parses");
        assert!(args.is_explain());
        assert_eq!(args.explain_rule.as_deref(), Some("BAMTS-W017"));
        let explanation = explain_rule(args.explain_rule.as_deref().unwrap()).expect("known rule");
        assert!(explanation.contains("rationale:"));
        assert!(explanation.contains("sound alternative:"));
        assert!(explanation.contains("silence: -A explicit-any"));
    }

    #[test]
    fn error_limit_rejects_zero() {
        assert!(matches!(
            parse_args(["check", "--error-limit", "0", "main.ts"]),
            Err(ArgsError::InvalidOptionValue { option, .. }) if option == "--error-limit"
        ));
    }
}
