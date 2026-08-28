//! Strict Test262 frontmatter, harness, phase, and async `$DONE` oracle.
//!
//! The interpreter backend runs in-process through the public `bamts` facade;
//! process-spawning backends stay outside. Callers may also supply typed
//! runner outcomes of their own.

use bamts_compiler::diagnostic::Diagnostic;
use bamts_runtime::{RuntimeErrorKind, ThrowOrigin};
use serde::Deserialize;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const FRONTMATTER_OPEN: &str = "/*---";
pub const FRONTMATTER_CLOSE: &str = "---*/";
pub const MAX_FRONTMATTER_BYTES: usize = 16_384;
pub const DEFAULT_ASYNC_DEADLINE: Duration = Duration::from_millis(500);

const HARNESS_ASSERT: &str = "assert.js";
const HARNESS_STA: &str = "sta.js";
const HARNESS_DONE: &str = "doneprintHandle.js";
const STRICT_PREFIX: &str = "\"use strict\";\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OracleError {
    MissingDelimiter,
    UnterminatedFrontmatter,
    OversizeFrontmatter {
        bytes: usize,
    },
    Yaml(String),
    UnknownFlag(String),
    ConflictingFlags,
    UnknownNegativePhase(String),
    UnknownNegativeType(String),
    IncludeNotConfined {
        include: String,
    },
    MissingHarness {
        name: String,
    },
    ModeFailure(ModeFailure),
    Done(DoneFailure),
    ExpectationMismatch {
        expected: String,
        actual: String,
    },
    /// The runner could not observe the script at all. Always blocking.
    BlockedRun {
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeFailure {
    pub mode: ExecutionMode,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoneFailure {
    Error,
    Duplicate,
    Missing,
    Late,
    EarlyExit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExecutionMode {
    Interpreter,
    Jit,
    Aot,
}

impl ExecutionMode {
    pub const ALL: [Self; 3] = [Self::Interpreter, Self::Jit, Self::Aot];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    OnlyStrict,
    NoStrict,
    Module,
    Raw,
    Async,
    Generated,
    CanBlockIsFalse,
    CanBlockIsTrue,
    NonDeterministic,
}

impl Flag {
    pub fn parse(name: &str) -> Result<Self, OracleError> {
        match name {
            "onlyStrict" => Ok(Self::OnlyStrict),
            "noStrict" => Ok(Self::NoStrict),
            "module" => Ok(Self::Module),
            "raw" => Ok(Self::Raw),
            "async" => Ok(Self::Async),
            "generated" => Ok(Self::Generated),
            "CanBlockIsFalse" => Ok(Self::CanBlockIsFalse),
            "CanBlockIsTrue" => Ok(Self::CanBlockIsTrue),
            "non-deterministic" => Ok(Self::NonDeterministic),
            other => Err(OracleError::UnknownFlag(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegativePhase {
    Parse,
    Resolution,
    Runtime,
}

impl NegativePhase {
    pub fn parse(name: &str) -> Result<Self, OracleError> {
        match name {
            "parse" => Ok(Self::Parse),
            "resolution" => Ok(Self::Resolution),
            "runtime" => Ok(Self::Runtime),
            other => Err(OracleError::UnknownNegativePhase(other.to_owned())),
        }
    }

    pub const fn is_early(self) -> bool {
        matches!(self, Self::Parse | Self::Resolution)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegativeType {
    SyntaxError,
    TypeError,
    ReferenceError,
    RangeError,
    UriError,
    EvalError,
    Test262Error,
}

impl NegativeType {
    pub fn parse(name: &str) -> Result<Self, OracleError> {
        match name {
            "SyntaxError" => Ok(Self::SyntaxError),
            "TypeError" => Ok(Self::TypeError),
            "ReferenceError" => Ok(Self::ReferenceError),
            "RangeError" => Ok(Self::RangeError),
            "URIError" => Ok(Self::UriError),
            "EvalError" => Ok(Self::EvalError),
            "Test262Error" => Ok(Self::Test262Error),
            other => Err(OracleError::UnknownNegativeType(other.to_owned())),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SyntaxError => "SyntaxError",
            Self::TypeError => "TypeError",
            Self::ReferenceError => "ReferenceError",
            Self::RangeError => "RangeError",
            Self::UriError => "URIError",
            Self::EvalError => "EvalError",
            Self::Test262Error => "Test262Error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negative {
    pub phase: NegativePhase,
    pub error_type: NegativeType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    pub description: Option<String>,
    pub info: Option<String>,
    pub esid: Option<String>,
    pub es5id: Option<String>,
    pub es6id: Option<String>,
    pub features: Vec<String>,
    pub flags: Vec<Flag>,
    pub includes: Vec<String>,
    pub locale: Vec<String>,
    pub author: Option<String>,
    pub negative: Option<Negative>,
}

impl Frontmatter {
    pub fn has(&self, flag: Flag) -> bool {
        self.flags.contains(&flag)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedFrontmatter<'a> {
    pub yaml: &'a str,
    pub body: &'a str,
    pub close_end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTest {
    pub source_bytes: Vec<u8>,
    pub yaml: String,
    pub frontmatter: Frontmatter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    ScriptNonStrict,
    ScriptStrict,
    Module,
    Raw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionVariant {
    pub kind: SourceKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessPlan {
    None,
    Canonical {
        files: Vec<ConfinedInclude>,
        async_done: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfinedInclude {
    pub requested: String,
    pub confined: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionPlan {
    pub variants: Vec<ExecutionVariant>,
    pub harness: HarnessPlan,
    pub negative: Option<Negative>,
    pub async_done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedScript {
    pub bytes: Vec<u8>,
    pub test_offset: usize,
    pub kind: SourceKind,
    pub untouched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessSource {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    pub mode: ExecutionMode,
    pub variant: ExecutionVariant,
    pub script: ComposedScript,
    pub negative: Option<Negative>,
    pub async_done: bool,
    pub deadline: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThrownError {
    pub phase: NegativePhase,
    pub error_type: NegativeType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoneEventKind {
    Success,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoneEvent {
    pub kind: DoneEventKind,
    pub at: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncTrace {
    pub events: Vec<DoneEvent>,
    pub exited_at: Option<Duration>,
    pub deadline: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Completed {
        thrown: Option<ThrownError>,
    },
    Async(AsyncTrace),
    /// No verdict-shaped observation was produced. This blocks the
    /// obligation; it can never judge as a pass.
    Blocked {
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllModesReport {
    pub modes: [ExecutionMode; 3],
}

pub trait Test262Runner {
    fn run(&self, request: &RunRequest) -> RunOutcome;
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrontmatterYaml {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    info: Option<String>,
    #[serde(default)]
    esid: Option<String>,
    #[serde(default)]
    es5id: Option<String>,
    #[serde(default)]
    es6id: Option<String>,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default)]
    flags: Vec<String>,
    #[serde(default)]
    includes: Vec<String>,
    #[serde(default)]
    locale: Vec<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    negative: Option<NegativeYaml>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NegativeYaml {
    phase: String,
    #[serde(rename = "type")]
    error_type: String,
}

fn yaml_options() -> serde_saphyr::Options {
    serde_saphyr::options! {
        duplicate_keys: serde_saphyr::DuplicateKeyPolicy::Error,
        merge_keys: serde_saphyr::MergeKeyPolicy::Error,
        budget: serde_saphyr::budget! {
            max_reader_input_bytes: Some(MAX_FRONTMATTER_BYTES),
            max_events: 8_192,
            max_aliases: 0,
            max_anchors: 0,
            max_depth: 16,
            max_inclusion_depth: 0,
            max_documents: 1,
            max_nodes: 1_024,
            max_total_scalar_bytes: MAX_FRONTMATTER_BYTES,
            max_merge_keys: 0,
        },
    }
}

pub fn extract_frontmatter(source: &str) -> Result<ExtractedFrontmatter<'_>, OracleError> {
    let open = source
        .find(FRONTMATTER_OPEN)
        .ok_or(OracleError::MissingDelimiter)?;
    let yaml_start = open + FRONTMATTER_OPEN.len();
    let relative_close = source[yaml_start..]
        .find(FRONTMATTER_CLOSE)
        .ok_or(OracleError::UnterminatedFrontmatter)?;
    let yaml_end = yaml_start + relative_close;
    let close_end = yaml_end + FRONTMATTER_CLOSE.len();
    let yaml = source[yaml_start..yaml_end].trim();
    if yaml.len() > MAX_FRONTMATTER_BYTES {
        return Err(OracleError::OversizeFrontmatter { bytes: yaml.len() });
    }
    Ok(ExtractedFrontmatter {
        yaml,
        body: &source[close_end..],
        close_end,
    })
}

pub fn parse_frontmatter_yaml(yaml: &str) -> Result<Frontmatter, OracleError> {
    if yaml.len() > MAX_FRONTMATTER_BYTES {
        return Err(OracleError::OversizeFrontmatter { bytes: yaml.len() });
    }
    let raw: FrontmatterYaml = serde_saphyr::from_str_with_options(yaml, yaml_options())
        .map_err(|error| OracleError::Yaml(error.to_string()))?;
    let mut flags = Vec::with_capacity(raw.flags.len());
    for flag in &raw.flags {
        flags.push(Flag::parse(flag)?);
    }
    let negative = match raw.negative {
        Some(value) => Some(Negative {
            phase: NegativePhase::parse(&value.phase)?,
            error_type: NegativeType::parse(&value.error_type)?,
        }),
        None => None,
    };
    Ok(Frontmatter {
        description: raw.description,
        info: raw.info,
        esid: raw.esid,
        es5id: raw.es5id,
        es6id: raw.es6id,
        features: raw.features,
        flags,
        includes: raw.includes,
        locale: raw.locale,
        author: raw.author,
        negative,
    })
}

pub fn parse_test(source: &str) -> Result<ParsedTest, OracleError> {
    let extracted = extract_frontmatter(source)?;
    let frontmatter = parse_frontmatter_yaml(extracted.yaml)?;
    Ok(ParsedTest {
        source_bytes: source.as_bytes().to_vec(),
        yaml: extracted.yaml.to_owned(),
        frontmatter,
    })
}

pub fn confine_include(harness_root: &Path, include: &str) -> Result<ConfinedInclude, OracleError> {
    if include.is_empty()
        || include.contains("..")
        || include.contains('\\')
        || include.contains('\0')
    {
        return Err(OracleError::IncludeNotConfined {
            include: include.to_owned(),
        });
    }
    let relative = Path::new(include);
    if relative.is_absolute() {
        return Err(OracleError::IncludeNotConfined {
            include: include.to_owned(),
        });
    }
    for component in relative.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                return Err(OracleError::IncludeNotConfined {
                    include: include.to_owned(),
                });
            }
        }
    }
    Ok(ConfinedInclude {
        requested: include.to_owned(),
        confined: harness_root.join(relative),
    })
}

fn flags_conflict(flags: &[Flag]) -> bool {
    flags.contains(&Flag::OnlyStrict) && flags.contains(&Flag::NoStrict)
}

pub fn plan_execution(
    frontmatter: &Frontmatter,
    harness_root: &Path,
) -> Result<ExecutionPlan, OracleError> {
    if flags_conflict(&frontmatter.flags) {
        return Err(OracleError::ConflictingFlags);
    }
    let raw = frontmatter.has(Flag::Raw);
    let module = frontmatter.has(Flag::Module);
    let only_strict = frontmatter.has(Flag::OnlyStrict);
    let no_strict = frontmatter.has(Flag::NoStrict);
    let async_done = frontmatter.has(Flag::Async);
    if raw && (module || only_strict) {
        return Err(OracleError::ConflictingFlags);
    }

    let variants = if raw {
        vec![ExecutionVariant {
            kind: SourceKind::Raw,
        }]
    } else if module {
        vec![ExecutionVariant {
            kind: SourceKind::Module,
        }]
    } else if only_strict {
        vec![ExecutionVariant {
            kind: SourceKind::ScriptStrict,
        }]
    } else if no_strict {
        vec![ExecutionVariant {
            kind: SourceKind::ScriptNonStrict,
        }]
    } else {
        vec![
            ExecutionVariant {
                kind: SourceKind::ScriptNonStrict,
            },
            ExecutionVariant {
                kind: SourceKind::ScriptStrict,
            },
        ]
    };

    let harness = if raw {
        HarnessPlan::None
    } else {
        let mut files = Vec::new();
        files.push(confine_include(harness_root, HARNESS_ASSERT)?);
        files.push(confine_include(harness_root, HARNESS_STA)?);
        if async_done {
            files.push(confine_include(harness_root, HARNESS_DONE)?);
        }
        for include in &frontmatter.includes {
            if include == HARNESS_ASSERT || include == HARNESS_STA || include == HARNESS_DONE {
                continue;
            }
            files.push(confine_include(harness_root, include)?);
        }
        HarnessPlan::Canonical { files, async_done }
    };

    Ok(ExecutionPlan {
        variants,
        harness,
        negative: frontmatter.negative.clone(),
        async_done,
    })
}

pub fn compose_script(
    parsed: &ParsedTest,
    plan: &ExecutionPlan,
    variant: &ExecutionVariant,
    harness_sources: &[HarnessSource],
) -> Result<ComposedScript, OracleError> {
    let early = plan
        .negative
        .as_ref()
        .is_some_and(|negative| negative.phase.is_early());
    if early || variant.kind == SourceKind::Raw {
        return Ok(ComposedScript {
            bytes: parsed.source_bytes.clone(),
            test_offset: 0,
            kind: variant.kind,
            untouched: true,
        });
    }

    let mut prefix = Vec::new();
    if let HarnessPlan::Canonical { files, .. } = &plan.harness {
        for file in files {
            let source = harness_sources
                .iter()
                .find(|source| source.name == file.requested)
                .map(|source| source.bytes.as_slice())
                .ok_or_else(|| OracleError::MissingHarness {
                    name: file.requested.clone(),
                })?;
            prefix.extend_from_slice(source);
            if !source.ends_with(b"\n") {
                prefix.push(b'\n');
            }
        }
    }
    if variant.kind == SourceKind::ScriptStrict {
        prefix.extend_from_slice(STRICT_PREFIX.as_bytes());
    }
    let test_offset = prefix.len();
    prefix.extend_from_slice(&parsed.source_bytes);
    Ok(ComposedScript {
        bytes: prefix,
        test_offset,
        kind: variant.kind,
        untouched: false,
    })
}

pub fn judge_done(trace: &AsyncTrace) -> Result<(), DoneFailure> {
    if let Some(exited_at) = trace.exited_at
        && trace.events.is_empty()
        && exited_at <= trace.deadline
    {
        return Err(DoneFailure::EarlyExit);
    }
    match trace.events.as_slice() {
        [] => Err(DoneFailure::Missing),
        [first, rest @ ..] => {
            if first.at > trace.deadline {
                return Err(DoneFailure::Late);
            }
            if !rest.is_empty() {
                return Err(DoneFailure::Duplicate);
            }
            match first.kind {
                DoneEventKind::Error => Err(DoneFailure::Error),
                DoneEventKind::Success => Ok(()),
            }
        }
    }
}

pub fn judge_run(request: &RunRequest, outcome: &RunOutcome) -> Result<(), OracleError> {
    match outcome {
        RunOutcome::Async(trace) => {
            if !request.async_done {
                return Err(OracleError::ExpectationMismatch {
                    expected: "non-async".to_owned(),
                    actual: "async".to_owned(),
                });
            }
            judge_done(trace).map_err(OracleError::Done)
        }
        RunOutcome::Completed { thrown } => match (&request.negative, thrown) {
            (None, None) => {
                if request.async_done {
                    Err(OracleError::Done(DoneFailure::Missing))
                } else {
                    Ok(())
                }
            }
            (Some(expected), Some(actual)) => {
                if expected.phase == actual.phase && expected.error_type == actual.error_type {
                    Ok(())
                } else {
                    Err(OracleError::ExpectationMismatch {
                        expected: format!("{:?}/{}", expected.phase, expected.error_type.as_str()),
                        actual: format!("{:?}/{}", actual.phase, actual.error_type.as_str()),
                    })
                }
            }
            (Some(expected), None) => Err(OracleError::ExpectationMismatch {
                expected: format!("{:?}/{}", expected.phase, expected.error_type.as_str()),
                actual: "completed".to_owned(),
            }),
            (None, Some(actual)) => Err(OracleError::ExpectationMismatch {
                expected: "success".to_owned(),
                actual: format!("{:?}/{}", actual.phase, actual.error_type.as_str()),
            }),
        },
        RunOutcome::Blocked { detail } => Err(OracleError::BlockedRun {
            detail: detail.clone(),
        }),
    }
}

pub fn evaluate_in_all_modes<R: Test262Runner>(
    runner: &R,
    parsed: &ParsedTest,
    plan: &ExecutionPlan,
    harness_sources: &[HarnessSource],
    deadline: Duration,
) -> Result<AllModesReport, OracleError> {
    for mode in ExecutionMode::ALL {
        for variant in &plan.variants {
            let script = compose_script(parsed, plan, variant, harness_sources)?;
            let request = RunRequest {
                mode,
                variant: variant.clone(),
                script,
                negative: plan.negative.clone(),
                async_done: plan.async_done,
                deadline,
            };
            let outcome = runner.run(&request);
            if let Err(error) = judge_run(&request, &outcome) {
                return Err(OracleError::ModeFailure(ModeFailure {
                    mode,
                    detail: format!("{error:?}"),
                }));
            }
        }
    }
    Ok(AllModesReport {
        modes: ExecutionMode::ALL,
    })
}

/// Why an execution backend cannot serve this oracle's in-process runs.
///
/// Every variant is a hard block: the obligation is recorded as blocked and
/// can never become a pass. No native-code backend is wired into this leaf,
/// so declaring the block costs no codegen dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedBackend {
    /// The JIT backend has no in-process runner in this oracle.
    Jit,
    /// The AOT backend has no in-process runner in this oracle.
    Aot,
}

impl BlockedBackend {
    /// The execution mode this block applies to.
    #[must_use]
    pub const fn mode(self) -> ExecutionMode {
        match self {
            Self::Jit => ExecutionMode::Jit,
            Self::Aot => ExecutionMode::Aot,
        }
    }

    /// Why the backend cannot run.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Jit => "the JIT backend is not wired for in-process oracle runs",
            Self::Aot => "the AOT backend is not wired for in-process oracle runs",
        }
    }
}

/// Returns the production runner for `mode`.
///
/// Only [`ExecutionMode::Interpreter`] executes here, through the in-process
/// engine facade. The native-code modes return a typed [`BlockedBackend`]
/// instead of a runner that could never honestly observe them.
pub fn backend_runner(
    mode: ExecutionMode,
    scratch: impl Into<PathBuf>,
) -> Result<InterpreterRunner, BlockedBackend> {
    match mode {
        ExecutionMode::Interpreter => Ok(InterpreterRunner::new(scratch)),
        ExecutionMode::Jit => Err(BlockedBackend::Jit),
        ExecutionMode::Aot => Err(BlockedBackend::Aot),
    }
}

/// The production Test262 runner: the in-process bytecode interpreter.
///
/// One [`RunRequest`] becomes one engine run. The composed script is
/// materialized verbatim under `scratch`, compiled through the public `bamts`
/// facade, and executed by `bamts_runtime::run` against the deterministic
/// Node host. Fuel is derived from the request deadline.
///
/// Classification is exactly what this boundary can observe:
///
/// - compile diagnostics in the lexical (`BAMTS-L`) or parser (`BAMTS-P`)
///   code families are a parse-phase `SyntaxError`;
/// - a runtime [`ThrowOrigin`] that names a native error class reports that
///   class;
/// - a guest-thrown value (`ThrowOrigin::Bytecode`) has no observable
///   constructor through the public runtime boundary, so the outcome is
///   [`RunOutcome::Blocked`] rather than a guessed type. The block can never
///   judge as a pass;
/// - exhausted fuel or a failed engine pipeline is likewise
///   [`RunOutcome::Blocked`]: no observation was made, so none is reported.
#[derive(Debug)]
pub struct InterpreterRunner {
    scratch: PathBuf,
    composed: AtomicU64,
}

/// Removes a materialized script on every exit path, including early `?`-less
/// classification returns and panics.
struct MaterializedScript(PathBuf);

impl Drop for MaterializedScript {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl InterpreterRunner {
    /// Creates a runner that materializes composed scripts under `scratch`.
    ///
    /// The directory is created on demand; the caller owns its lifetime.
    #[must_use]
    pub fn new(scratch: impl Into<PathBuf>) -> Self {
        Self {
            scratch: scratch.into(),
            composed: AtomicU64::new(0),
        }
    }
}

impl Test262Runner for InterpreterRunner {
    fn run(&self, request: &RunRequest) -> RunOutcome {
        let foreign = match request.mode {
            ExecutionMode::Interpreter => None,
            ExecutionMode::Jit => Some(BlockedBackend::Jit.reason()),
            ExecutionMode::Aot => Some(BlockedBackend::Aot.reason()),
        };
        if let Some(reason) = foreign {
            return RunOutcome::Blocked {
                detail: reason.to_owned(),
            };
        }

        if let Err(error) = fs::create_dir_all(&self.scratch) {
            return RunOutcome::Blocked {
                detail: format!(
                    "could not create scratch directory `{}`: {error}",
                    self.scratch.display()
                ),
            };
        }
        let unique = self.composed.fetch_add(1, Ordering::Relaxed);
        let script = MaterializedScript(
            self.scratch
                .join(format!("composed-{}-{unique}.js", std::process::id())),
        );
        if let Err(error) = fs::write(&script.0, &request.script.bytes) {
            return RunOutcome::Blocked {
                detail: format!(
                    "could not materialize composed script `{}`: {error}",
                    script.0.display()
                ),
            };
        }

        let executable = match bamts::compile_source_file(&script.0) {
            Ok(executable) => executable,
            Err(bamts::Error::Diagnostics { diagnostics }) => {
                return parse_phase_outcome(&diagnostics);
            }
            Err(error) => {
                return RunOutcome::Blocked {
                    detail: format!("compile pipeline failed: {error}"),
                };
            }
        };

        let started = Instant::now();
        let mut host = bamts_node::NodeHost::new();
        let outcome = bamts_runtime::run(
            executable.wire(),
            &mut host,
            &interpreter_fuel(request.deadline),
        );
        let elapsed = started.elapsed();
        match outcome {
            Ok(_) if request.async_done => RunOutcome::Async(observed_done_trace(
                host.stdout(),
                host.stderr(),
                elapsed,
                request.deadline,
            )),
            Ok(_) => RunOutcome::Completed { thrown: None },
            Err(error) => observed_failure(&error, request.deadline),
        }
    }
}

/// Classifies compile diagnostics into a thrown parse error, or blocks when
/// the diagnostics are not parse-shaped.
///
/// Only the lexical (`BAMTS-L`) and parser (`BAMTS-P`) code families are
/// JavaScript parse errors. Any other diagnostic means the oracle pipeline
/// rejected source that Test262 considers valid, which is an observation
/// failure and never a Test262 verdict.
fn parse_phase_outcome(diagnostics: &[Diagnostic]) -> RunOutcome {
    let parse_shaped = diagnostics.iter().any(|diagnostic| {
        let code = diagnostic.code().as_str();
        code.starts_with("BAMTS-L") || code.starts_with("BAMTS-P")
    });
    if parse_shaped {
        return RunOutcome::Completed {
            thrown: Some(ThrownError {
                phase: NegativePhase::Parse,
                error_type: NegativeType::SyntaxError,
            }),
        };
    }
    let codes = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code().as_str())
        .collect::<Vec<_>>()
        .join(", ");
    RunOutcome::Blocked {
        detail: format!(
            "compile produced {} non-parse diagnostic(s): {codes}",
            diagnostics.len()
        ),
    }
}

/// Maps an engine throw origin onto the Test262 negative type it names.
///
/// [`ThrowOrigin::Bytecode`] is a guest-thrown value; its constructor is not
/// observable through the public runtime boundary, so there is no mapping.
#[must_use]
fn classify_thrown(origin: ThrowOrigin) -> Option<NegativeType> {
    match origin {
        ThrowOrigin::TypeError { .. } => Some(NegativeType::TypeError),
        ThrowOrigin::RangeError { .. } => Some(NegativeType::RangeError),
        ThrowOrigin::ReferenceError { .. } => Some(NegativeType::ReferenceError),
        ThrowOrigin::UriError { .. } => Some(NegativeType::UriError),
        ThrowOrigin::Bytecode => None,
    }
}

/// Classifies an engine failure into a thrown runtime error or a block.
fn observed_failure(error: &bamts_runtime::RuntimeError, deadline: Duration) -> RunOutcome {
    match &error.kind {
        RuntimeErrorKind::UncaughtThrow { origin, .. } => match classify_thrown(*origin) {
            Some(error_type) => RunOutcome::Completed {
                thrown: Some(ThrownError {
                    phase: NegativePhase::Runtime,
                    error_type,
                }),
            },
            None => {
                let site = error
                    .source
                    .function_name
                    .as_ref()
                    .map_or_else(|| "<anonymous>".to_owned(), |name| name.to_utf8_lossy());
                RunOutcome::Blocked {
                    detail: format!(
                        "guest threw a value whose constructor is unobservable at this \
                         boundary (throw site: {site})"
                    ),
                }
            }
        },
        RuntimeErrorKind::FuelExhausted { .. } => RunOutcome::Blocked {
            detail: format!("fuel exhausted within the {deadline:?} deadline"),
        },
        _ => RunOutcome::Blocked {
            detail: format!("runtime failed without a throw: {error}"),
        },
    }
}

/// Converts a wall-clock deadline into deterministic interpreter fuel.
///
/// The interpreter has no preemption, so fuel checked at instruction
/// boundaries is the deadline's deterministic proxy, matching the corpus
/// runner's budget.
#[must_use]
fn interpreter_fuel(deadline: Duration) -> bamts_runtime::Limits {
    const FUEL_PER_MILLISECOND: u64 = 10_000;
    let milliseconds = u64::try_from(deadline.as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    bamts_runtime::Limits {
        fuel: milliseconds.saturating_mul(FUEL_PER_MILLISECOND),
        ..bamts_runtime::Limits::default()
    }
}

/// Observes `$DONE` markers in the captured host output.
///
/// `$DONE()` records a success; `$DONE(` followed by any other byte records
/// an error. Every marker is stamped with the post-run elapsed time, and
/// [`judge_done`] owns every ordering rule.
#[must_use]
fn observed_done_trace(
    stdout: &[u8],
    stderr: &[u8],
    elapsed: Duration,
    deadline: Duration,
) -> AsyncTrace {
    let mut events = Vec::new();
    for stream in [stdout, stderr] {
        let mut cursor = 0;
        while let Some(found) = find_marker(&stream[cursor..], b"$DONE(") {
            let at = cursor + found;
            let kind = if stream.get(at + MARKER_OPEN.len()) == Some(&b')') {
                DoneEventKind::Success
            } else {
                DoneEventKind::Error
            };
            events.push(DoneEvent { kind, at: elapsed });
            cursor = at + MARKER_OPEN.len();
        }
    }
    AsyncTrace {
        events,
        exited_at: Some(elapsed),
        deadline,
    }
}

/// The `$DONE(` invocation prefix in Test262 harness output.
const MARKER_OPEN: &[u8] = b"$DONE(";

fn find_marker(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::super::test262_harness::{FileHarnessLoader, load_harness_sources};

    static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique per-test scratch directory for harness files and materialized
    /// composed scripts.
    fn runner_scratch(tag: &str) -> PathBuf {
        let unique = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bamts-test262-{tag}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create runner scratch dir");
        dir
    }

    fn write_harness(root: &Path, name: &str, contents: &str) {
        fs::write(root.join(name), contents).expect("write harness include");
    }

    /// Minimal clean-room stand-ins for the canonical Test262 harness files:
    /// enough of `assert`, `$ERROR`, `Test262Error`, and `$DONE` for these
    /// fixtures, with none of the upstream bodies.
    const ASSERT_JS: &str = "var assert;\n\
(function () {\n\
  assert = {\n\
    sameValue: function (actual, expected, message) {\n\
      if (actual !== expected) {\n\
        $ERROR(message);\n\
      }\n\
    },\n\
  };\n\
})();\n";

    const STA_JS: &str = "function Test262Error(message) {\n\
  this.message = message;\n\
}\n\
function $ERROR(message) {\n\
  throw new Test262Error(message);\n\
}\n";

    const DONEPRINT_JS: &str = "function $DONE(failure) {\n\
  if (failure) {\n\
    console.log('$DONE(' + String(failure) + ')');\n\
  } else {\n\
    console.log('$DONE()');\n\
  }\n\
}\n";

    /// The production interpreter flow for one variant: compose, request,
    /// run in-process, and judge. Returns the judge verdict.
    fn run_variant(
        runner: &InterpreterRunner,
        parsed: &ParsedTest,
        plan: &ExecutionPlan,
        sources: &[HarnessSource],
        variant: &ExecutionVariant,
    ) -> Result<(), OracleError> {
        let script = compose_script(parsed, plan, variant, sources)?;
        let request = RunRequest {
            mode: ExecutionMode::Interpreter,
            variant: variant.clone(),
            script,
            negative: plan.negative.clone(),
            async_done: plan.async_done,
            deadline: DEFAULT_ASYNC_DEADLINE,
        };
        let outcome = runner.run(&request);
        judge_run(&request, &outcome)
    }

    fn wrap(yaml: &str, body: &str) -> String {
        format!("{FRONTMATTER_OPEN}\n{yaml}\n{FRONTMATTER_CLOSE}\n{body}")
    }

    fn harness_root() -> PathBuf {
        PathBuf::from("/pinned/harness")
    }

    fn harness_sources() -> Vec<HarnessSource> {
        vec![
            HarnessSource {
                name: HARNESS_ASSERT.to_owned(),
                bytes: b"/*assert*/\n".to_vec(),
            },
            HarnessSource {
                name: HARNESS_STA.to_owned(),
                bytes: b"/*sta*/\n".to_vec(),
            },
            HarnessSource {
                name: HARNESS_DONE.to_owned(),
                bytes: b"/*done*/\n".to_vec(),
            },
            HarnessSource {
                name: "propertyHelper.js".to_owned(),
                bytes: b"/*helper*/\n".to_vec(),
            },
        ]
    }

    struct PassRunner;

    impl Test262Runner for PassRunner {
        fn run(&self, request: &RunRequest) -> RunOutcome {
            if request.async_done {
                RunOutcome::Async(AsyncTrace {
                    events: vec![DoneEvent {
                        kind: DoneEventKind::Success,
                        at: Duration::from_millis(1),
                    }],
                    exited_at: None,
                    deadline: request.deadline,
                })
            } else if let Some(negative) = &request.negative {
                RunOutcome::Completed {
                    thrown: Some(ThrownError {
                        phase: negative.phase,
                        error_type: negative.error_type,
                    }),
                }
            } else {
                RunOutcome::Completed { thrown: None }
            }
        }
    }

    struct FailJitRunner;

    impl Test262Runner for FailJitRunner {
        fn run(&self, request: &RunRequest) -> RunOutcome {
            if request.mode == ExecutionMode::Jit {
                RunOutcome::Completed {
                    thrown: Some(ThrownError {
                        phase: NegativePhase::Runtime,
                        error_type: NegativeType::TypeError,
                    }),
                }
            } else {
                PassRunner.run(request)
            }
        }
    }

    #[test]
    fn frontmatter_is_strict_and_bounded() {
        let block = wrap(
            "description: block yaml\nflags: [noStrict]\ninfo: |\n  line one\n  line two",
            "void 0;",
        );
        let parsed = parse_test(&block).expect("block yaml");
        assert_eq!(
            parsed.frontmatter.description.as_deref(),
            Some("block yaml")
        );
        assert_eq!(
            parsed.frontmatter.info.as_deref().map(str::trim),
            Some("line one\nline two")
        );
        assert!(parsed.frontmatter.has(Flag::NoStrict));

        let flow = wrap(
            "{description: flow, flags: [onlyStrict, async], includes: [propertyHelper.js]}",
            "$DONE();",
        );
        let parsed = parse_test(&flow).expect("flow yaml");
        assert_eq!(parsed.frontmatter.description.as_deref(), Some("flow"));
        assert!(parsed.frontmatter.has(Flag::OnlyStrict));
        assert!(parsed.frontmatter.has(Flag::Async));
        assert_eq!(parsed.frontmatter.includes, ["propertyHelper.js"]);

        let quoted = wrap("description: \"quoted scalar\"\nauthor: 'nobody'", "");
        let parsed = parse_test(&quoted).expect("quoted scalars");
        assert_eq!(
            parsed.frontmatter.description.as_deref(),
            Some("quoted scalar")
        );
        assert_eq!(parsed.frontmatter.author.as_deref(), Some("nobody"));

        let duplicate = wrap("description: one\ndescription: two", "");
        match parse_test(&duplicate) {
            Err(OracleError::Yaml(_)) => {}
            other => panic!("duplicate keys must fail, got {other:?}"),
        }

        let unknown = wrap("description: x\nmystery: true", "");
        match parse_test(&unknown) {
            Err(OracleError::Yaml(_)) => {}
            other => panic!("unknown fields must fail, got {other:?}"),
        }

        assert!(matches!(
            extract_frontmatter("no delimiter here"),
            Err(OracleError::MissingDelimiter)
        ));
        assert!(matches!(
            extract_frontmatter("/*---\nbad: true\n"),
            Err(OracleError::UnterminatedFrontmatter)
        ));

        let huge = "x".repeat(MAX_FRONTMATTER_BYTES + 1);
        match extract_frontmatter(&format!("/*---\n{huge}\n---*/\n")) {
            Err(OracleError::OversizeFrontmatter { bytes }) => {
                assert!(bytes > MAX_FRONTMATTER_BYTES);
            }
            other => panic!("oversize must fail, got {other:?}"),
        }
    }

    #[test]
    fn includes_are_confined_and_ordered() {
        let source = wrap(
            "description: includes\nincludes: [propertyHelper.js]",
            "void 0;",
        );
        let parsed = parse_test(&source).unwrap();
        let plan = plan_execution(&parsed.frontmatter, &harness_root()).unwrap();
        let HarnessPlan::Canonical { files, async_done } = plan.harness else {
            panic!("canonical harness required");
        };
        assert!(!async_done);
        let names: Vec<&str> = files.iter().map(|file| file.requested.as_str()).collect();
        assert_eq!(names, ["assert.js", "sta.js", "propertyHelper.js"]);
        for file in &files {
            assert!(file.confined.starts_with(harness_root()));
        }

        let traversal = wrap("description: traversal\nincludes: [../secret.js]", "");
        let parsed = parse_test(&traversal).unwrap();
        assert!(matches!(
            plan_execution(&parsed.frontmatter, &harness_root()),
            Err(OracleError::IncludeNotConfined { .. })
        ));

        let absolute = wrap("description: abs\nincludes: [/etc/passwd]", "");
        let parsed = parse_test(&absolute).unwrap();
        assert!(matches!(
            plan_execution(&parsed.frontmatter, &harness_root()),
            Err(OracleError::IncludeNotConfined { .. })
        ));

        assert!(matches!(
            confine_include(Path::new("/pinned/harness"), "..\\escape.js"),
            Err(OracleError::IncludeNotConfined { .. })
        ));
    }

    #[test]
    fn early_negative_phase_and_type_are_exact() {
        for (phase, ty, yaml_phase, yaml_type) in [
            (
                NegativePhase::Parse,
                NegativeType::SyntaxError,
                "parse",
                "SyntaxError",
            ),
            (
                NegativePhase::Resolution,
                NegativeType::ReferenceError,
                "resolution",
                "ReferenceError",
            ),
            (
                NegativePhase::Runtime,
                NegativeType::TypeError,
                "runtime",
                "TypeError",
            ),
        ] {
            let flags = if phase == NegativePhase::Resolution {
                "flags: [module]\n"
            } else {
                "flags: [raw]\n"
            };
            let source = wrap(
                &format!(
                    "description: negative\n{flags}negative:\n  phase: {yaml_phase}\n  type: {yaml_type}"
                ),
                "throw 1;",
            );
            let parsed = parse_test(&source).unwrap();
            let negative = parsed.frontmatter.negative.clone().expect("negative");
            assert_eq!(negative.phase, phase);
            assert_eq!(negative.error_type, ty);
            let plan = plan_execution(&parsed.frontmatter, &harness_root()).unwrap();
            let variant = &plan.variants[0];
            let script = compose_script(&parsed, &plan, variant, &harness_sources())
                .expect("compose negative");
            if phase.is_early() {
                assert!(script.untouched);
                assert_eq!(script.bytes, parsed.source_bytes);
                assert_eq!(script.test_offset, 0);
            }
            let request = RunRequest {
                mode: ExecutionMode::Interpreter,
                variant: variant.clone(),
                script,
                negative: plan.negative.clone(),
                async_done: plan.async_done,
                deadline: DEFAULT_ASYNC_DEADLINE,
            };
            let outcome = PassRunner.run(&request);
            judge_run(&request, &outcome).expect("exact negative match");
        }
    }

    #[test]
    fn runtime_harness_preserves_observables() {
        let source = wrap(
            "description: runtime\nincludes: [propertyHelper.js]",
            "observable();",
        );
        let parsed = parse_test(&source).unwrap();
        let plan = plan_execution(&parsed.frontmatter, &harness_root()).unwrap();
        let variant = plan
            .variants
            .iter()
            .find(|variant| variant.kind == SourceKind::ScriptNonStrict)
            .expect("non-strict variant");
        let script =
            compose_script(&parsed, &plan, variant, &harness_sources()).expect("compose runtime");
        assert!(!script.untouched);
        let prefix = std::str::from_utf8(&script.bytes[..script.test_offset]).unwrap();
        assert!(prefix.contains("/*assert*/"));
        assert!(prefix.contains("/*sta*/"));
        assert!(prefix.contains("/*helper*/"));
        assert_eq!(
            &script.bytes[script.test_offset..],
            parsed.source_bytes.as_slice()
        );
        assert!(
            std::str::from_utf8(&script.bytes)
                .unwrap()
                .contains("observable();")
        );

        let strict = plan
            .variants
            .iter()
            .find(|variant| variant.kind == SourceKind::ScriptStrict)
            .expect("strict variant");
        let strict_script =
            compose_script(&parsed, &plan, strict, &harness_sources()).expect("compose strict");
        let before_test = &strict_script.bytes[..strict_script.test_offset];
        assert!(before_test.ends_with(STRICT_PREFIX.as_bytes()));
        assert_eq!(
            &strict_script.bytes[strict_script.test_offset..],
            parsed.source_bytes.as_slice()
        );
    }

    #[test]
    fn done_protocol_is_exact() {
        let deadline = Duration::from_millis(10);
        let success = AsyncTrace {
            events: vec![DoneEvent {
                kind: DoneEventKind::Success,
                at: Duration::from_millis(1),
            }],
            exited_at: None,
            deadline,
        };
        assert!(judge_done(&success).is_ok());

        let error = AsyncTrace {
            events: vec![DoneEvent {
                kind: DoneEventKind::Error,
                at: Duration::from_millis(1),
            }],
            exited_at: None,
            deadline,
        };
        assert_eq!(judge_done(&error), Err(DoneFailure::Error));

        let duplicate = AsyncTrace {
            events: vec![
                DoneEvent {
                    kind: DoneEventKind::Success,
                    at: Duration::from_millis(1),
                },
                DoneEvent {
                    kind: DoneEventKind::Success,
                    at: Duration::from_millis(2),
                },
            ],
            exited_at: None,
            deadline,
        };
        assert_eq!(judge_done(&duplicate), Err(DoneFailure::Duplicate));

        let missing = AsyncTrace {
            events: vec![],
            exited_at: None,
            deadline,
        };
        assert_eq!(judge_done(&missing), Err(DoneFailure::Missing));

        let late = AsyncTrace {
            events: vec![DoneEvent {
                kind: DoneEventKind::Success,
                at: Duration::from_millis(11),
            }],
            exited_at: None,
            deadline,
        };
        assert_eq!(judge_done(&late), Err(DoneFailure::Late));

        let early = AsyncTrace {
            events: vec![],
            exited_at: Some(Duration::from_millis(1)),
            deadline,
        };
        assert_eq!(judge_done(&early), Err(DoneFailure::EarlyExit));
    }

    #[test]
    fn flags_select_exact_plan() {
        let root = harness_root();
        let default = parse_test(&wrap("description: default", "")).unwrap();
        let plan = plan_execution(&default.frontmatter, &root).unwrap();
        assert_eq!(
            plan.variants
                .iter()
                .map(|variant| variant.kind)
                .collect::<Vec<_>>(),
            [SourceKind::ScriptNonStrict, SourceKind::ScriptStrict]
        );

        let only = parse_test(&wrap("description: s\nflags: [onlyStrict]", "")).unwrap();
        let plan = plan_execution(&only.frontmatter, &root).unwrap();
        assert_eq!(plan.variants[0].kind, SourceKind::ScriptStrict);

        let no = parse_test(&wrap("description: n\nflags: [noStrict]", "")).unwrap();
        let plan = plan_execution(&no.frontmatter, &root).unwrap();
        assert_eq!(plan.variants[0].kind, SourceKind::ScriptNonStrict);

        let module = parse_test(&wrap("description: m\nflags: [module]", "")).unwrap();
        let plan = plan_execution(&module.frontmatter, &root).unwrap();
        assert_eq!(plan.variants[0].kind, SourceKind::Module);
        assert!(matches!(plan.harness, HarnessPlan::Canonical { .. }));

        let raw = parse_test(&wrap("description: r\nflags: [raw]", "")).unwrap();
        let plan = plan_execution(&raw.frontmatter, &root).unwrap();
        assert_eq!(plan.variants[0].kind, SourceKind::Raw);
        assert_eq!(plan.harness, HarnessPlan::None);

        let async_mod = parse_test(&wrap("description: a\nflags: [async, module]", "")).unwrap();
        let plan = plan_execution(&async_mod.frontmatter, &root).unwrap();
        assert!(plan.async_done);
        let HarnessPlan::Canonical { files, async_done } = plan.harness else {
            panic!("async module uses harness");
        };
        assert!(async_done);
        let names: Vec<&str> = files.iter().map(|file| file.requested.as_str()).collect();
        assert_eq!(names, ["assert.js", "sta.js", "doneprintHandle.js"]);

        let raw_strict =
            parse_test(&wrap("description: bad\nflags: [raw, onlyStrict]", "")).unwrap();
        assert!(matches!(
            plan_execution(&raw_strict.frontmatter, &root),
            Err(OracleError::ConflictingFlags)
        ));
    }

    #[test]
    fn requires_all_execution_modes() {
        let source = wrap("description: modes\nflags: [noStrict]", "void 0;");
        let parsed = parse_test(&source).unwrap();
        let plan = plan_execution(&parsed.frontmatter, &harness_root()).unwrap();
        let report = evaluate_in_all_modes(
            &PassRunner,
            &parsed,
            &plan,
            &harness_sources(),
            DEFAULT_ASYNC_DEADLINE,
        )
        .expect("all modes pass");
        assert_eq!(
            report.modes,
            [
                ExecutionMode::Interpreter,
                ExecutionMode::Jit,
                ExecutionMode::Aot
            ]
        );

        match evaluate_in_all_modes(
            &FailJitRunner,
            &parsed,
            &plan,
            &harness_sources(),
            DEFAULT_ASYNC_DEADLINE,
        ) {
            Err(OracleError::ModeFailure(failure)) => {
                assert_eq!(failure.mode, ExecutionMode::Jit);
            }
            other => panic!("JIT failure must be recorded, got {other:?}"),
        }
    }

    #[test]
    fn interpreter_runner_passes_positive_same_value_fixture() {
        let harness = runner_scratch("pos-harness");
        let scratch = runner_scratch("pos-scratch");
        write_harness(&harness, "assert.js", ASSERT_JS);
        write_harness(&harness, "sta.js", STA_JS);
        let source = wrap(
            "description: assert.sameValue passes on identical primitives",
            "assert.sameValue(1, 1, 'one is one');",
        );
        let parsed = parse_test(&source).unwrap();
        let plan = plan_execution(&parsed.frontmatter, &harness).unwrap();
        let sources = load_harness_sources(&plan, &FileHarnessLoader).unwrap();

        // Strict frontmatter is preserved: the strict variant still composes
        // the directive directly before the test body.
        assert_eq!(plan.variants.len(), 2);
        let strict = compose_script(&parsed, &plan, &plan.variants[1], &sources).unwrap();
        assert_eq!(strict.kind, SourceKind::ScriptStrict);
        assert_eq!(
            &strict.bytes[strict.test_offset - STRICT_PREFIX.len()..strict.test_offset],
            STRICT_PREFIX.as_bytes()
        );

        let runner = backend_runner(ExecutionMode::Interpreter, &scratch).unwrap();
        for variant in &plan.variants {
            run_variant(&runner, &parsed, &plan, &sources, variant).unwrap();
        }
    }

    #[test]
    fn interpreter_runner_judges_runtime_reference_error_fixture() {
        let harness = runner_scratch("neg-runtime-harness");
        let scratch = runner_scratch("neg-runtime-scratch");
        write_harness(&harness, "assert.js", ASSERT_JS);
        write_harness(&harness, "sta.js", STA_JS);
        let source = wrap(
            "description: unresolvable reference throws\n\
             negative:\n  phase: runtime\n  type: ReferenceError",
            "unresolvableReference;",
        );
        let parsed = parse_test(&source).unwrap();
        assert_eq!(
            parsed.frontmatter.negative,
            Some(Negative {
                phase: NegativePhase::Runtime,
                error_type: NegativeType::ReferenceError,
            })
        );
        let plan = plan_execution(&parsed.frontmatter, &harness).unwrap();
        let sources = load_harness_sources(&plan, &FileHarnessLoader).unwrap();
        let runner = backend_runner(ExecutionMode::Interpreter, &scratch).unwrap();
        for variant in &plan.variants {
            run_variant(&runner, &parsed, &plan, &sources, variant).unwrap();
        }
    }

    #[test]
    fn interpreter_runner_judges_parse_syntax_error_fixture() {
        let harness = runner_scratch("neg-parse-harness");
        let scratch = runner_scratch("neg-parse-scratch");
        write_harness(&harness, "assert.js", ASSERT_JS);
        write_harness(&harness, "sta.js", STA_JS);
        let source = wrap(
            "description: a numeric literal is not a binding target\n\
             negative:\n  phase: parse\n  type: SyntaxError",
            "var 1 = 1;",
        );
        let parsed = parse_test(&source).unwrap();
        let plan = plan_execution(&parsed.frontmatter, &harness).unwrap();
        let sources = load_harness_sources(&plan, &FileHarnessLoader).unwrap();
        let runner = backend_runner(ExecutionMode::Interpreter, &scratch).unwrap();
        for variant in &plan.variants {
            let script = compose_script(&parsed, &plan, variant, &sources).unwrap();
            assert!(script.untouched, "early negatives compose untouched");
            run_variant(&runner, &parsed, &plan, &sources, variant).unwrap();
        }
    }

    #[test]
    fn interpreter_runner_judges_async_done_fixture() {
        let harness = runner_scratch("async-harness");
        let scratch = runner_scratch("async-scratch");
        write_harness(&harness, "assert.js", ASSERT_JS);
        write_harness(&harness, "sta.js", STA_JS);
        write_harness(&harness, "doneprintHandle.js", DONEPRINT_JS);
        let source = wrap(
            "description: an async test completes through $DONE\nflags: [async]",
            "$DONE();",
        );
        let parsed = parse_test(&source).unwrap();
        assert!(parsed.frontmatter.has(Flag::Async));
        let plan = plan_execution(&parsed.frontmatter, &harness).unwrap();
        let sources = load_harness_sources(&plan, &FileHarnessLoader).unwrap();
        let runner = backend_runner(ExecutionMode::Interpreter, &scratch).unwrap();
        for variant in &plan.variants {
            run_variant(&runner, &parsed, &plan, &sources, variant).unwrap();
        }
    }

    #[test]
    fn unobservable_guest_throws_block_the_run() {
        let harness = runner_scratch("guest-harness");
        let scratch = runner_scratch("guest-scratch");
        write_harness(&harness, "assert.js", ASSERT_JS);
        write_harness(&harness, "sta.js", STA_JS);
        let source = wrap(
            "description: a guest throw carries no observable type",
            "throw new Test262Error('unmatched');",
        );
        let parsed = parse_test(&source).unwrap();
        let plan = plan_execution(&parsed.frontmatter, &harness).unwrap();
        let sources = load_harness_sources(&plan, &FileHarnessLoader).unwrap();
        let runner = backend_runner(ExecutionMode::Interpreter, &scratch).unwrap();
        for variant in &plan.variants {
            let result = run_variant(&runner, &parsed, &plan, &sources, variant);
            assert!(
                matches!(result, Err(OracleError::BlockedRun { .. })),
                "a guest throw must block, never pass or mismatch: {result:?}"
            );
        }
    }

    #[test]
    fn fuel_exhaustion_blocks_the_run() {
        let harness = runner_scratch("fuel-harness");
        let scratch = runner_scratch("fuel-scratch");
        write_harness(&harness, "assert.js", ASSERT_JS);
        write_harness(&harness, "sta.js", STA_JS);
        let source = wrap(
            "description: an endless loop never completes",
            "for (;;) {}",
        );
        let parsed = parse_test(&source).unwrap();
        let plan = plan_execution(&parsed.frontmatter, &harness).unwrap();
        let sources = load_harness_sources(&plan, &FileHarnessLoader).unwrap();
        let runner = backend_runner(ExecutionMode::Interpreter, &scratch).unwrap();
        for variant in &plan.variants {
            let script = compose_script(&parsed, &plan, variant, &sources).unwrap();
            let request = RunRequest {
                mode: ExecutionMode::Interpreter,
                variant: variant.clone(),
                script,
                negative: plan.negative.clone(),
                async_done: plan.async_done,
                deadline: Duration::from_millis(1),
            };
            let outcome = runner.run(&request);
            assert!(matches!(
                judge_run(&request, &outcome),
                Err(OracleError::BlockedRun { .. })
            ));
        }
    }

    #[test]
    fn native_backends_report_typed_blocks() {
        let scratch = runner_scratch("blocked-scratch");
        for (mode, blocked) in [
            (ExecutionMode::Jit, BlockedBackend::Jit),
            (ExecutionMode::Aot, BlockedBackend::Aot),
        ] {
            assert_eq!(backend_runner(mode, &scratch).unwrap_err(), blocked);
            assert_eq!(blocked.mode(), mode);
            assert!(!blocked.reason().is_empty());
        }
    }
}
