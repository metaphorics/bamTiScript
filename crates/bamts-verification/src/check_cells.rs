//! S2 check-cell machinery: the `diagnostics` facet executor plus the case
//! pragmas, multi-unit splitting, and flat-baseline resolution the U2.8 S2
//! facets share.
//!
//! One check cell materializes its case's virtual unit files into a per-cell
//! temp directory, compiles through the real [`ProgramLoader`] +
//! [`compile_program_frontend`] pipeline (the one real module resolver and the
//! canonical diagnostics union), and compares the emitted observations against
//! the upstream baseline via the `facets.rs` comparators.
//!
//! Baseline resolution is flat by stem (`tests/baselines/reference/<stem>…`):
//! duplicate stems are disambiguated by the baseline's first
//! `//// [<full case logical path>] ////` ownership marker, and the
//! `<stem>(<option>=<value>)` suffix names a variant compile.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Write as _,
    fs,
    path::Path,
};

use bamts_bytecode::EcmaString;
use bamts_compiler::checker::{SemanticModel, SymbolId, SymbolKind, Type, TypeId};
use bamts_compiler::diagnostic::DiagnosticSeverity;
use bamts_compiler::pipeline::{
    FrontendMode, FrontendOutput, ProgramFrontendOutput, compile_program_frontend,
};
use bamts_compiler::program::{ProgramLoader, ResolvedProgram};
use bamts_compiler::project::{ProjectConfig, ProjectRoot};
use bamts_compiler::source::SourceText;
use bamts_compiler::source::{TextRange, Utf16Pos};
use bamts_compiler::syntax::{NodeId, Token, TokenKind};

use crate::facets::{
    DiagnosticCategory, DiagnosticCodeMap, FacetDiagnostic, FacetSeverity, FacetVerdict,
    SourcePosition, compare_diagnostics, compare_symbols, compare_types,
};
use crate::suite::{
    CellResult, FailureClass, IndexEntry, PlannedCell, SuiteIndex, SuiteSnapshot, TempDir,
    decode_case_source,
};
use crate::{ErrorCode, Result, VerificationError};

/// Baselines are imported flat under `tests/baselines/reference/`; the runner
/// stores every non-case blob under the snapshot's `baselines/` content store.
const BASELINE_REFERENCE_PREFIX: &str = "tests/baselines/reference/";

/// Everything one check cell needs beyond the snapshot: loaded once per run.
pub struct CheckContext {
    /// The validated BAMTS↔TS diagnostic correspondence table.
    pub code_map: DiagnosticCodeMap,
    /// Stem/suffix baseline groups built once from the snapshot index, so the
    /// per-cell executors do not rescan the entire index on every invocation.
    pub baseline_groups: BaselineGroups,
}

/// One unit of a case blob: the split at each `// @filename:` directive.
///
/// Single-unit cases yield one unit named by the case's own logical path.
/// Names are virtual: relative names are joined under the case's directory,
/// absolute names (`/ref.d.ts`) are remapped under the temp root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseUnit {
    /// Root-relative virtual path (`foo/testB.ts`).
    pub virtual_path: String,
    /// UTF-8 unit text (BOMs handled by `decode_case_source` upstream).
    pub text: String,
}

/// Pragmas parsed from `// @<name>: <value>` case directive lines.
///
/// Option names are lowercased to match `(name=value)` baseline suffixes; a
/// comma-separated value lists the option's variants in declaration order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CasePragmas {
    /// `(option, values)` pairs in declaration order.
    pub options: Vec<(String, Vec<String>)>,
    /// `@noTypesAndSymbols: true` suppresses types/symbols baseline output.
    pub no_types_and_symbols: bool,
}

/// One file-anchored error row of an `.errors.txt` baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorsDiagnostic {
    /// The virtual unit name exactly as printed by the baseline (`b.ts`).
    pub unit: String,
    /// 1-based line.
    pub line: u32,
    /// 0-based UTF-16 character (baseline prints 1-based; stored minus one).
    pub character: u32,
    /// `error` or `warning` from the baseline text.
    pub category: String,
    /// TypeScript code (`TS2305`).
    pub code: String,
    /// Message text after `TS<code>: `.
    pub message: String,
}

/// One file-less (`error TS5107: …`) header row of an `.errors.txt` baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalDiagnostic {
    /// TypeScript code (`TS5107`).
    pub code: String,
    /// Message text.
    pub message: String,
}

/// Parsed `.errors.txt` baseline: the file-anchored rows the comparator sees
/// plus the file-less global rows reported separately.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorsBaseline {
    /// `file(l,c): (error|warning) TSnnnn: message` rows, in baseline order.
    pub diagnostics: Vec<ErrorsDiagnostic>,
    /// File-less global rows (option deprecation etc.); the compiler has no
    /// corresponding observation yet, so these are excluded from comparison.
    pub globals: Vec<GlobalDiagnostic>,
}

impl ErrorsBaseline {
    /// Number of file-anchored diagnostics.
    #[must_use]
    pub fn file_diagnostic_count(&self) -> usize {
        self.diagnostics.len()
    }
}

/// The resolved diagnostics-baseline view of one case: which (if any)
/// `.errors.txt` baselines belong to it.
#[derive(Debug, Clone, Default)]
pub struct DiagnosticsBaselines {
    /// Baselines whose ownership marker names this case, or that carry no
    /// marker when this case is the stem's sole input. None ⇒ upstream
    /// expects zero diagnostics (the strictest row class).
    pub owned: Vec<String>,
}

/// The per-cell product of compiling one materialized case.
pub struct CheckedCase {
    /// Units in split order paired with the index of their frontend module in
    /// [`ProgramFrontendOutput::modules`]. Unreachable units (never imported
    /// from the entry) carry no module index.
    pub units: Vec<(CaseUnit, Option<usize>)>,
    /// Virtual path of the entry unit (the unit that was loaded first).
    pub entry: String,
    /// Whole-program frontend product (one module per resolved file).
    output: ProgramFrontendOutput,
}

impl CheckedCase {
    /// The frontend output of one unit, when the program reached it.
    #[must_use]
    pub fn module_output(&self, unit: &CaseUnit) -> Option<&FrontendOutput> {
        self.units
            .iter()
            .find(|(candidate, _)| candidate.virtual_path == unit.virtual_path)
            .and_then(|(_, index)| index.map(|index| &self.output.modules()[index]))
    }

    /// `(unit, output)` pairs for every unit the program reached.
    pub fn reached_units(&self) -> impl Iterator<Item = (&CaseUnit, &FrontendOutput)> {
        self.units
            .iter()
            .filter_map(|(unit, index)| index.map(|index| (unit, &self.output.modules()[index])))
    }
}

/// The execution verdict of one diagnostics check cell.
#[derive(Debug, Clone)]
pub struct DiagnosticsOutcome {
    /// Diagnostics emitted by the compile (`collect_facet_diagnostics`).
    pub actual: Vec<FacetDiagnostic>,
    /// Diagnostics expected by the resolved baselines (union of all owned
    /// `.errors.txt` rows; empty when no baseline owns the case).
    pub expected: Vec<FacetDiagnostic>,
    /// The comparator verdict.
    pub verdict: FacetVerdict,
}

/// Failure to compile a case for a check cell.
#[derive(Debug)]
pub enum CaseCompileError {
    /// The loader rejected the program (module resolution, root confinement,
    /// unreadable entrypoint). Reported as `FAIL_BEHAVIOR` with the loader's
    /// own text as evidence.
    Load(String),
    /// A unit's text exceeds the frontend's source bound. `ProgramLoader`
    /// reports these as `Read` errors; this variant covers the harness-side
    /// pre-check. Reported as `FAIL_BEHAVIOR`.
    SourceBound(String),
    /// The compiler panicked inside `catch_unwind`. Reported as `CRASH`.
    Panic(String),
}

impl CaseCompileError {
    /// The cell's failure class per the U2.8 discipline (loader/bound errors
    /// are S5-triage behavior failures; panics are crashes).
    #[must_use]
    pub const fn failure_class(&self) -> FailureClass {
        match self {
            Self::Load(_) | Self::SourceBound(_) => FailureClass::FailBehavior,
            Self::Panic(_) => FailureClass::Crash,
        }
    }
}

/// Parse the `// @<name>: <value>` directive block of a case text.
///
/// Only whole-line directives are recognized. `@filename:` units are not
/// options; everything else is recorded with a lowercased name, comma-split
/// values, and `@noTypesAndSymbols: true` pinned to the suppression flag.
pub fn parse_case_pragmas(text: &str) -> CasePragmas {
    let mut options = Vec::new();
    let mut no_types_and_symbols = false;
    for line in text.lines() {
        let Some(directive) = directive_body(line) else {
            continue;
        };
        let Some((name, value)) = directive.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "filename" {
            continue;
        }
        if name == "notypesandsymbols" {
            no_types_and_symbols |= value.eq_ignore_ascii_case("true");
            continue;
        }
        let values: Vec<String> = value
            .split(',')
            .map(|part| part.trim().to_owned())
            .filter(|part| !part.is_empty())
            .collect();
        options.push((name, values));
    }
    CasePragmas {
        options,
        no_types_and_symbols,
    }
}

/// Returns the body of a whole-line `// @name: …` directive, if the line is one.
fn directive_body(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("//")?;
    let rest = rest.trim_start_matches('/').trim_start();
    rest.strip_prefix('@')
}

/// Strip every whole-line `// @name: value` compiler-option directive from a
/// unit's compiled content, reproducing upstream `makeUnitsFromTest`.
///
/// TypeScript removes each such line (global options and `@filename:` markers
/// alike) before compiling, joins the surviving lines with `\n`, and — because
/// the first accumulated chunk starts empty — collapses the leading blank lines
/// that a stripped prologue leaves behind. Reproducing all three keeps the
/// compiled source, and therefore every emitted `.errors.txt`/`.types`/
/// `.symbols` position, numbered exactly as the baselines are.
fn strip_directive_lines(text: &str) -> String {
    // `Utils.splitContentByNewlines` splits on `\r\n` when the file has any,
    // else on `\n`; the surviving lines rejoin with `\n`, normalizing endings.
    let lines: Vec<&str> = if text.contains("\r\n") {
        text.split("\r\n").collect()
    } else {
        text.split('\n').collect()
    };
    let mut content: Option<String> = None;
    for line in lines {
        if is_option_directive_line(line) {
            continue;
        }
        match &mut content {
            None => content = Some(line.to_owned()),
            Some(current) => {
                if !current.is_empty() {
                    current.push('\n');
                }
                current.push_str(line);
            }
        }
    }
    content.unwrap_or_default()
}

/// Whether a terminator-free line matches upstream's option-directive grammar
/// `^//\s*@\w+\s*:` — exactly two leading slashes, a single ASCII-word option
/// name, then a colon. This is the exact set upstream strips from unit content;
/// `///` reference directives and `@word` lines without a colon are kept.
fn is_option_directive_line(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("//") else {
        return false;
    };
    // A third leading slash is a `///` directive, not a `// @option`.
    if rest.starts_with('/') {
        return false;
    }
    let rest = rest.trim_start();
    let Some(rest) = rest.strip_prefix('@') else {
        return false;
    };
    let name_len = rest
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        .count();
    if name_len == 0 {
        return false;
    }
    rest[name_len..].trim_start().starts_with(':')
}

/// Split a case blob at `// @filename:` boundaries.
///
/// A directive must be a whole-line comment. Text before the first directive
/// belongs to the unit named by `logical_path`'s basename; a directive naming
/// the case's own basename is also treated as that first unit (no second
/// empty unit). Unit virtual paths are joined under the case's virtual
/// directory; absolute directive names are remapped to the root.
pub fn split_case_units(logical_path: &str, text: &str) -> Vec<CaseUnit> {
    let case_name = logical_path.rsplit('/').next().unwrap_or(logical_path);
    let case_dir = logical_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");

    // Gather directive markers as (line start byte offset, unit name).
    let mut markers: Vec<(usize, String)> = Vec::new();
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let body = directive_body(line.trim_end_matches(['\n', '\r']));
        let name = body.and_then(|body| {
            let (name, value) = body.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("filename")
                .then(|| value.trim().to_owned())
        });
        if let Some(name) = name.filter(|name| !name.is_empty()) {
            markers.push((line_start, name));
        }
    }

    let mut units: Vec<CaseUnit> = Vec::new();
    let push_unit = |name: String, text: &str, units: &mut Vec<CaseUnit>| {
        let virtual_path = virtual_unit_path(case_dir, &name);
        // Upstream `makeUnitsFromTest` strips every `// @name: value` directive
        // line (global options and `@filename:` markers alike) from a unit's
        // compiled content, so a case's `.errors.txt`, `.types`, and
        // `.symbols` positions are numbered over directive-free source. Strip
        // the same lines here so emitted positions align with the baselines.
        let content = strip_directive_lines(text);
        // Later directives that merely restate an earlier unit's name append
        // to that unit, mirroring upstream's `test.get("units")` naming.
        if let Some(existing) = units.iter_mut().find(|u| u.virtual_path == virtual_path) {
            existing.text.push_str(&content);
        } else {
            units.push(CaseUnit {
                virtual_path,
                text: content,
            });
        }
    };

    if markers.is_empty() {
        push_unit(case_name.to_owned(), text, &mut units);
        return units;
    }

    // Drop a marker that names the case's own basename at position 0: it
    // identifies the entry unit, it does not start a second one.
    let first_names_case = markers
        .first()
        .is_some_and(|(_, name)| name.rsplit('/').next() == Some(case_name));

    let mut body_start = 0usize;
    let mut body_name: Option<String> = None;
    for (index, (marker_start, name)) in markers.iter().enumerate() {
        if index == 0 && first_names_case {
            // The leading marker names the case's own basename: preamble and
            // the marker line both belong to the entry unit.
            body_start = 0;
            body_name = Some(case_name.to_owned());
            continue;
        }
        let end = *marker_start;
        match &body_name {
            Some(name) => push_unit(name.clone(), &text[body_start..end], &mut units),
            None => {
                if end > 0 {
                    push_unit(case_name.to_owned(), &text[..end], &mut units);
                }
            }
        }
        body_start = *marker_start;
        body_name = Some(name.clone());
    }
    if let Some(name) = body_name {
        push_unit(name, &text[body_start..], &mut units);
    }
    if units.is_empty() {
        push_unit(case_name.to_owned(), text, &mut units);
    }
    units
}

/// Map a `@filename:` name onto a root-relative virtual path: relative names
/// join under the case's directory; absolute virtual names (`/ref.d.ts`) are
/// remapped to the root so `ProgramLoader` can confine them.
fn virtual_unit_path(case_dir: &str, name: &str) -> String {
    let name = name.replace('\\', "/");
    if let Some(rest) = name.strip_prefix('/') {
        return rest.to_owned();
    }
    if case_dir.is_empty() {
        name
    } else {
        format!("{case_dir}/{name}")
    }
}
/// Whether a virtual unit path names a TypeScript declaration file.
fn is_check_cell_declaration_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".d.ts") || lower.ends_with(".d.mts") || lower.ends_with(".d.cts")
}

/// Group snapshot baseline index entries for the flat
/// `tests/baselines/reference/` store by `(stem, extension, suffix)`.
///
/// Key: `(stem, extension)` where extension is `errors.txt`/`types`/`symbols`;
/// value: `(suffix, logical_path)` where suffix is the `(opt=value)` group or
/// empty for the plain baseline.
pub type BaselineGroups = BTreeMap<(String, String), Vec<(String, String)>>;

/// Build the stem/suffix groups once from the snapshot index (flat entries
/// only; `reference/project/…` rows are excluded).
pub fn baseline_groups(index: &SuiteIndex) -> BaselineGroups {
    let mut groups: BaselineGroups = BTreeMap::new();
    for entry in index.entries.values() {
        let Some(file_name) = entry.logical_path.strip_prefix(BASELINE_REFERENCE_PREFIX) else {
            continue;
        };
        if file_name.contains('/') {
            continue;
        }
        let Some((stem, extension, suffix)) = split_baseline_file_name(file_name) else {
            continue;
        };
        groups
            .entry((stem.to_owned(), extension.to_owned()))
            .or_default()
            .push((suffix.to_owned(), entry.logical_path.clone()));
    }
    groups
}

/// Split `<stem>[(suffix)].<ext>`; returns `None` for unrecognized names.
fn split_baseline_file_name(file_name: &str) -> Option<(&str, &str, &str)> {
    let extensions = [".errors.txt", ".types", ".symbols"];
    let extension = extensions
        .iter()
        .find(|extension| file_name.ends_with(*extension))?;
    let base = &file_name[..file_name.len() - extension.len()];
    if let Some(open) = base.find('(') {
        let suffix = &base[open..];
        if !suffix.ends_with(')') {
            return None;
        }
        Some((&base[..open], &extension[1..], suffix))
    } else {
        Some((base, &extension[1..], ""))
    }
}

/// Parse a `(opt=value,...)` baseline suffix and check whether it matches the
/// first-value compile options used for the current case.
fn suffix_matches_options(suffix: &str, compile_options: &[(String, String)]) -> bool {
    let Some(inner) = suffix.strip_prefix('(').and_then(|s| s.strip_suffix(')')) else {
        return suffix.is_empty();
    };
    if inner.is_empty() {
        return true;
    }
    let options: std::collections::HashMap<String, String> = compile_options
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    for part in inner.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((key, value)) = part.split_once('=') else {
            return false;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_ascii_lowercase();
        let Some(compile_value) = options.get(&key) else {
            return false;
        };
        if compile_value.to_ascii_lowercase() != value {
            return false;
        }
    }
    true
}

/// The stem of a case logical path: basename minus extension, with `.d` kept
/// (`parserEnumDeclaration2.d.ts` ⇒ stem `parserEnumDeclaration2.d`).
pub fn case_stem(logical_path: &str) -> &str {
    let name = logical_path.rsplit('/').next().unwrap_or(logical_path);
    for suffix in [".tsx", ".ts", ".jsx", ".js", ".mts", ".cts", ".mjs", ".cjs"] {
        if let Some(stem) = name.strip_suffix(suffix) {
            return stem;
        }
    }
    name
}

/// Resolve the `.errors.txt` baselines owned by one case, filtered to the
/// variant that matches the compile options actually used.
///
/// A baseline is owned when its first `//// [path] ////` marker names the
/// case's full logical path; a marker-less baseline is owned only when the
/// case is the stem's sole case input in the index. When variant baselines
/// exist (e.g. `(target=es5)`), only the variant matching the compile options
/// is selected; the plain baseline is used as a fallback when no variant matches.
pub fn resolve_errors_baselines(
    snapshot: &SuiteSnapshot,
    groups: &BaselineGroups,
    logical_path: &str,
    compile_options: &[(String, String)],
) -> DiagnosticsBaselines {
    let stem = case_stem(logical_path);
    let Some(candidates) = groups.get(&(stem.to_owned(), "errors.txt".to_owned())) else {
        return DiagnosticsBaselines::default();
    };
    let sole_input = case_inputs_with_stem(snapshot, stem) == [logical_path];
    let mut variants = Vec::new();
    let mut plain = Vec::new();
    for (suffix, path) in candidates {
        let owned = match baseline_owner(snapshot, path) {
            Some(owner) => owner == logical_path,
            None => sole_input,
        };
        if !owned {
            continue;
        }
        if suffix.is_empty() {
            plain.push(path.clone());
        } else if suffix_matches_options(suffix, compile_options) {
            variants.push(path.clone());
        }
    }
    let mut owned = if !variants.is_empty() {
        variants
    } else {
        plain
    };
    owned.sort();
    DiagnosticsBaselines { owned }
}

/// The case inputs (compiler/conformance/project/projects only matter here)
/// whose stem equals `stem`.
fn case_inputs_with_stem<'a>(snapshot: &'a SuiteSnapshot, stem: &str) -> Vec<&'a str> {
    snapshot
        .index
        .entries
        .values()
        .filter(|entry| {
            matches!(entry.asset_kind, crate::suite::AssetKind::CaseInput)
                && case_stem(&entry.logical_path) == stem
        })
        .map(|entry| entry.logical_path.as_str())
        .collect()
}

/// The first `//// [path] ////` marker of a baseline blob, if any.
fn baseline_owner(snapshot: &SuiteSnapshot, logical_path: &str) -> Option<String> {
    let entry = snapshot.index.entries.get(logical_path)?;
    let blob = snapshot.root.join("baselines").join(&entry.sha256);
    let text = fs::read_to_string(blob).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("//// [") {
            return rest
                .split_once("] ////")
                .map(|(path, _)| path.trim().to_owned());
        }
        if !line.is_empty() {
            // Markers only lead the document; stop at the first content line.
            return None;
        }
    }
    None
}

/// Parse an `.errors.txt` baseline into file-anchored and global rows.
///
/// Strict row grammar: `^<file>(<l>,<c>): (error|warning) TS\d+: <msg>` and
/// `^(error|warning) TS\d+: <msg>` for file-less globals. `!!!` summaries,
/// `====` source dumps, caret lines, and blank separators are skipped.
pub fn parse_errors_baseline(text: &str) -> ErrorsBaseline {
    let mut baseline = ErrorsBaseline::default();
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(' ') || line.starts_with('\t') {
            // Source dumps and caret lines are indented.
            continue;
        }
        if line.starts_with("!!!") || line.starts_with("====") {
            continue;
        }
        if let Some(diagnostic) = parse_file_row(line) {
            baseline.diagnostics.push(diagnostic);
        } else if let Some(global) = parse_global_row(line) {
            baseline.globals.push(global);
        }
    }
    baseline
}

/// Parse `file(l,c): (error|warning) TS\d+: message`.
fn parse_file_row(line: &str) -> Option<ErrorsDiagnostic> {
    let open = line.find('(')?;
    let (unit, rest) = line.split_at(open);
    let unit = unit.trim();
    if unit.is_empty() {
        return None;
    }
    let rest = rest.strip_prefix('(')?;
    let (position, rest) = rest.split_once("): ")?;
    let (line_no, character) = position.split_once(',')?;
    let line_no: u32 = line_no.trim().parse().ok()?;
    let character: u32 = character.trim().parse().ok()?;
    let (category, code, message) = parse_category_code(rest)?;
    Some(ErrorsDiagnostic {
        unit: unit.to_owned(),
        line: line_no,
        character: character.saturating_sub(1),
        category,
        code,
        message,
    })
}

/// Parse `(error|warning) TS\d+: message` (no file anchor).
fn parse_global_row(line: &str) -> Option<GlobalDiagnostic> {
    let (_, code, message) = parse_category_code(line)?;
    Some(GlobalDiagnostic { code, message })
}

/// Parse the shared `<category> TS<code>: <message>` tail.
fn parse_category_code(rest: &str) -> Option<(String, String, String)> {
    let (category, rest) = rest.split_once(' ')?;
    if !matches!(category, "error" | "warning") {
        return None;
    }
    let (code, message) = rest.split_once(": ")?;
    let digits = code.strip_prefix("TS")?;
    if digits.is_empty() || !digits.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some((category.to_owned(), code.to_owned(), message.to_owned()))
}

/// Compile one materialized case (all units) into its checked product.
///
/// Materializes the units under a fresh temp root, loads the entry unit
/// through [`ProgramLoader`], and runs [`compile_program_frontend`] in check
/// mode. All units are written even though the loader only reads the ones
/// reachable from the entry's module graph, so `.d.ts`/library-style units
/// Build a JSONC `tsconfig.json` source from a case's `@option` pragmas.
/// Upstream cases default to `strict: true` unless the case explicitly says
/// otherwise, so the harness supplies that default when no `@strict` directive
/// is present. Only the first value of each comma-split option is used.
fn build_tsconfig(pragmas: &CasePragmas) -> String {
    let mut options: Vec<(String, String)> = Vec::new();
    for (name, values) in &pragmas.options {
        let Some(value) = values.first() else {
            continue;
        };
        let json = match name.as_str() {
            "strict"
            | "strictnullchecks"
            | "noimplicitany"
            | "allowjs"
            | "checkjs"
            | "resolvejsonmodule"
            | "noemit"
            | "declaration"
            | "alwaysstrict"
            | "exactoptionalpropertytypes"
            | "nouncheckedindexedaccess" => value.eq_ignore_ascii_case("true").to_string(),
            _ => format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")),
        };
        let key = match name.as_str() {
            "strictnullchecks" => "strictNullChecks",
            "noimplicitany" => "noImplicitAny",
            "allowjs" => "allowJs",
            "checkjs" => "checkJs",
            "resolvejsonmodule" => "resolveJsonModule",
            "moduleresolution" => "moduleResolution",
            "noemit" => "noEmit",
            "alwaysstrict" => "alwaysStrict",
            "exactoptionalpropertytypes" => "exactOptionalPropertyTypes",
            "nouncheckedindexedaccess" => "noUncheckedIndexedAccess",
            _ => name.as_str(),
        };
        options.push((key.to_owned(), json));
    }
    if !options.iter().any(|(name, _)| name == "strict") {
        options.push(("strict".to_owned(), "true".to_owned()));
    }
    let compiler_options = options
        .iter()
        .map(|(name, value)| format!("\"{name}\": {value}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut source = String::new();
    source.push('{');
    source.push_str("\"compilerOptions\": {");
    source.push_str(&compiler_options);
    source.push_str("}}");
    source
}

pub fn compile_case(
    units: &[CaseUnit],
    entry_name: &str,
) -> std::result::Result<CheckedCase, CaseCompileError> {
    compile_case_with_pragmas(units, entry_name, &CasePragmas::default())
}

pub fn compile_case_with_pragmas(
    units: &[CaseUnit],
    entry_name: &str,
    pragmas: &CasePragmas,
) -> std::result::Result<CheckedCase, CaseCompileError> {
    let temp = TempDir::new("bamts-check-cell")
        .map_err(|error| CaseCompileError::Load(format!("temp root: {error}")))?;
    for unit in units {
        if unit.text.len() > bamts_compiler::source::MAX_SOURCE_BYTES {
            return Err(CaseCompileError::SourceBound(format!(
                "unit `{}` exceeds the {}-byte frontend source bound",
                unit.virtual_path,
                bamts_compiler::source::MAX_SOURCE_BYTES
            )));
        }
        let path = temp.path().join(&unit.virtual_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| CaseCompileError::Load(format!("unit dir: {error}")))?;
        }
        fs::write(&path, &unit.text)
            .map_err(|error| CaseCompileError::Load(format!("unit write: {error}")))?;
    }
    // Upstream TypeScript test programs compile every unit together, not just
    // the ones reachable from a single entry. Create a synthetic entrypoint that
    // imports every unit so the S2 harness observes the same program shape.
    const ENTRY: &str = "__bamts_check_entry__.ts";
    let mut entry_source = String::new();
    for (index, unit) in units.iter().enumerate() {
        let specifier = unit.virtual_path.replace('\\', "/");
        if is_check_cell_declaration_path(&specifier) {
            entry_source.push_str(&format!(
                "import type * as _bamts_entry_{index} from \"./{specifier}\";\n"
            ));
        } else {
            entry_source.push_str(&format!("import \"./{specifier}\";\n"));
        }
    }
    fs::write(temp.path().join(ENTRY), entry_source)
        .map_err(|error| CaseCompileError::Load(format!("entry write: {error}")))?;
    let compile = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let root = ProjectRoot::new(temp.path())
            .map_err(|error| CaseCompileError::Load(format!("project root: {error}")))?;
        let config_source = build_tsconfig(pragmas);
        let config = ProjectConfig::parse(&root, temp.path().join("tsconfig.json"), &config_source)
            .map_err(|error| CaseCompileError::Load(format!("config: {error}")))?;
        let loader = ProgramLoader::new(&root, config.options())
            .map_err(|error| CaseCompileError::Load(format!("loader: {error}")))?;
        let program = loader
            .load(Path::new(ENTRY))
            .map_err(|error| CaseCompileError::Load(error.to_string()))?;
        let output = compile_program_frontend(&program, FrontendMode::Check);
        Ok::<_, CaseCompileError>((program, output))
    }));
    let (program, output) = match compile {
        Ok(Ok(parts)) => parts,
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            return Err(CaseCompileError::Panic(
                "compiler panicked while checking the case".to_owned(),
            ));
        }
    };
    let pairs = pair_units_with_modules(units, temp.path(), &program);
    // The synthetic entry is not part of the case, so it is intentionally absent
    // from `pairs`. Any diagnostics it produced are also ignored.
    Ok(CheckedCase {
        units: pairs,
        entry: entry_name.to_owned(),
        output,
    })
}

/// Match each unit to its resolved module by canonical path. The loader
/// canonicalizes every module path under the confined root, so unit paths are
/// canonicalized the same way before comparison.
fn pair_units_with_modules(
    units: &[CaseUnit],
    root: &Path,
    program: &ResolvedProgram,
) -> Vec<(CaseUnit, Option<usize>)> {
    units
        .iter()
        .map(|unit| {
            let canonical = fs::canonicalize(root.join(&unit.virtual_path)).ok();
            let index = canonical.and_then(|canonical| {
                program
                    .modules()
                    .iter()
                    .position(|module| module.path() == canonical)
            });
            (unit.clone(), index)
        })
        .collect()
}

/// The virtual path of the unit that acts as the compile entry: the first
/// split unit (single-unit cases and cases whose first `@filename` names the
/// case all lead with the entry unit).
pub fn entry_virtual_path(logical_path: &str, units: &[CaseUnit]) -> String {
    let _ = logical_path;
    units
        .first()
        .map(|unit| unit.virtual_path.clone())
        .unwrap_or_else(|| {
            logical_path
                .rsplit('/')
                .next()
                .unwrap_or(logical_path)
                .to_owned()
        })
}

/// Basename of a virtual path (`foo/b.ts` ⇒ `b.ts`), matching the unit name a
/// baseline prints so both sides of the diagnostics comparator agree.
fn unit_basename(virtual_path: &str) -> String {
    virtual_path
        .rsplit('/')
        .next()
        .unwrap_or(virtual_path)
        .to_owned()
}

/// Flatten a checked case into the diagnostics comparator's actual side.
///
/// Positions are 1-based line / 0-based UTF-16 character over the unit text;
/// BAMTS codes carry their `BAMTS-*` spelling for the code map, category
/// follows `error` vs `warning`, and severity mirrors the compiler's
/// error/warning split. The unit basename is carried so a cross-unit position
/// collision cannot pass the check.
pub fn collect_facet_diagnostics(case: &CheckedCase) -> Vec<FacetDiagnostic> {
    let mut diagnostics = Vec::new();
    for (unit, output) in case.reached_units() {
        let unit_name = unit_basename(&unit.virtual_path);
        let source = SourceText::new(unit.text.clone())
            .unwrap_or_else(|_| panic!("unit text was bound-checked before compile"));
        for diagnostic in output.diagnostics() {
            let (line, character) = source
                .line_column(diagnostic.range().start())
                .map(|(line, character)| ((line + 1) as u32, character as u32))
                .unwrap_or((0, 0));
            let severity = match diagnostic.severity() {
                DiagnosticSeverity::Error => FacetSeverity::Error,
                DiagnosticSeverity::Warning => FacetSeverity::Warning,
            };
            let category = match diagnostic.severity() {
                DiagnosticSeverity::Error => DiagnosticCategory::Error,
                DiagnosticSeverity::Warning => DiagnosticCategory::Warning,
            };
            diagnostics.push(FacetDiagnostic {
                unit: unit_name.clone(),
                position: SourcePosition { line, character },
                category,
                severity,
                code: diagnostic.code().as_str().to_owned(),
            });
        }
    }
    diagnostics
}

/// Union the owned `.errors.txt` baselines into the comparator's expected
/// side. File-less global rows stay out: the compiler has no global-diagnostic
/// observation yet (they name option-deprecation semantics owned by S7/S5).
/// The unit basename printed by the baseline is carried through so a
/// cross-unit position collision cannot pass the check.
pub fn expected_facet_diagnostics(
    snapshot: &SuiteSnapshot,
    baselines: &DiagnosticsBaselines,
) -> Vec<FacetDiagnostic> {
    let mut expected = Vec::new();
    for path in &baselines.owned {
        let Some(entry) = snapshot.index.entries.get(path) else {
            continue;
        };
        let blob = snapshot.root.join("baselines").join(&entry.sha256);
        let Ok(text) = fs::read_to_string(blob) else {
            continue;
        };
        let parsed = parse_errors_baseline(&text);
        for row in parsed.diagnostics {
            let category = if row.category == "warning" {
                DiagnosticCategory::Warning
            } else {
                DiagnosticCategory::Error
            };
            let severity = if row.category == "warning" {
                FacetSeverity::Warning
            } else {
                FacetSeverity::Error
            };
            expected.push(FacetDiagnostic {
                unit: row.unit,
                position: SourcePosition {
                    line: row.line,
                    character: row.character,
                },
                category,
                severity,
                code: row.code,
            });
        }
    }
    expected
}

/// Run the diagnostics comparison for one compiled case.
///
/// The compile uses the default lint profile, but TS error baselines carry no
/// lint output: actual diagnostics are filtered to the code map's BAMTS
/// L/P/C families (every mapped compiler diagnostic) before comparing, so
/// `BAMTS-W…` lint warnings never fail a row.
pub fn check_diagnostics(
    ctx: &CheckContext,
    snapshot: &SuiteSnapshot,
    case: &CheckedCase,
    baselines: &DiagnosticsBaselines,
) -> DiagnosticsOutcome {
    let mut actual = collect_facet_diagnostics(case);
    actual.retain(|diagnostic| ctx.code_map.get(&diagnostic.code).is_some());
    let expected = expected_facet_diagnostics(snapshot, baselines);
    let verdict = compare_diagnostics(&expected, &actual, &ctx.code_map);
    DiagnosticsOutcome {
        actual,
        expected,
        verdict,
    }
}

/// Execute the S2 `diagnostics` observation for one planned cell.
///
/// Failure-class discipline mirrors `execute_parse_check`: compile failures
/// are `FAIL_BEHAVIOR` (loader/bound) or `CRASH` (panic); comparator `Fail`
/// is `FAIL_DIAGNOSTIC`; `Unproven` is an oracle-side `HARNESS_ERROR`; a pass
/// records the emitted/expected counts in the detail.
pub(crate) fn execute_diagnostics_check(
    ctx: &CheckContext,
    snapshot: &SuiteSnapshot,
    plan: &PlannedCell,
    index_entry: &IndexEntry,
) -> Result<CellResult> {
    let blob = snapshot.root.join("cases").join(&index_entry.sha256);
    let bytes = fs::read(&blob).map_err(|error| {
        VerificationError::new(
            ErrorCode::Io,
            format!("cannot read case blob `{}`: {error}", blob.display()),
        )
    })?;
    let text = decode_case_source(&bytes);
    let pragmas = parse_case_pragmas(&text);
    let units = split_case_units(&index_entry.logical_path, &text);
    let entry = entry_virtual_path(&index_entry.logical_path, &units);
    let groups = &ctx.baseline_groups;
    let compile_options: Vec<(String, String)> = pragmas
        .options
        .iter()
        .filter_map(|(name, values)| values.first().map(|value| (name.clone(), value.clone())))
        .collect();
    let baselines = resolve_errors_baselines(
        snapshot,
        groups,
        &index_entry.logical_path,
        &compile_options,
    );
    let result = compile_case_with_pragmas(&units, &entry, &pragmas);
    let (class, detail) = match result {
        Err(error) => {
            let detail = match &error {
                CaseCompileError::Load(detail)
                | CaseCompileError::SourceBound(detail)
                | CaseCompileError::Panic(detail) => detail.clone(),
            };
            (error.failure_class(), detail)
        }
        Ok(case) => {
            let outcome = check_diagnostics(ctx, snapshot, &case, &baselines);
            match outcome.verdict {
                FacetVerdict::Pass => (
                    FailureClass::Pass,
                    format!(
                        "diagnostic parity: {} expected, {} emitted",
                        outcome.expected.len(),
                        outcome.actual.len()
                    ),
                ),
                FacetVerdict::Fail { reason } => (FailureClass::FailDiagnostic, reason),
                FacetVerdict::Unproven { reason } => (
                    FailureClass::HarnessError,
                    format!("diagnostics oracle could not prove parity: {reason}"),
                ),
            }
        }
    };
    Ok(CellResult {
        entry_id: plan.entry.id.clone(),
        facet: plan.entry.facet,
        backend: plan.backend,
        class,
        detail,
    })
}

/// Index-only baseline-ownership view for U2.8 S2 `types` classification.
///
/// The classifier (`build_classified_ledger`) has the index but not the blob
/// store, so ownership is decided from index facts alone: a compiler /
/// conformance input owns a `.types` baseline when one exists for its stem and
/// it is the sole case input carrying that stem. Duplicate stems (which need a
/// blob `//// [path] ////` marker to disambiguate) and stems with no `.types`
/// baseline (`APISample_*`, `@noTypesAndSymbols`) are therefore excluded.
pub struct S2Classification {
    /// Stems that have at least one `.types` baseline blob.
    types_stems: HashSet<String>,
    /// Stems that have at least one `.symbols` baseline blob.
    symbols_stems: HashSet<String>,
    /// Case-input count per stem (a duplicate stem has count > 1).
    stem_case_counts: HashMap<String, usize>,
}

impl S2Classification {
    /// Build the index-only ownership lookups once per classification pass.
    #[must_use]
    pub fn from_index(index: &SuiteIndex) -> Self {
        let mut types_stems = HashSet::new();
        let mut symbols_stems = HashSet::new();
        let mut stem_case_counts: HashMap<String, usize> = HashMap::new();
        for entry in index.entries.values() {
            match entry.asset_kind {
                crate::suite::AssetKind::CaseInput => {
                    *stem_case_counts
                        .entry(case_stem(&entry.logical_path).to_owned())
                        .or_default() += 1;
                }
                crate::suite::AssetKind::BaselineFacet => {
                    let Some(name) = entry.logical_path.strip_prefix(BASELINE_REFERENCE_PREFIX)
                    else {
                        continue;
                    };
                    if name.contains('/') {
                        continue;
                    }
                    match split_baseline_file_name(name) {
                        Some((stem, "types", _)) => {
                            types_stems.insert(stem.to_owned());
                        }
                        Some((stem, "symbols", _)) => {
                            symbols_stems.insert(stem.to_owned());
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        Self {
            types_stems,
            symbols_stems,
            stem_case_counts,
        }
    }

    /// Whether a compiler/conformance input uniquely owns a `.types` baseline.
    #[must_use]
    pub fn owns_types_baseline(&self, input: &str) -> bool {
        let stem = case_stem(input);
        self.types_stems.contains(stem) && self.stem_case_counts.get(stem).copied() == Some(1)
    }

    /// Whether a compiler/conformance input uniquely owns a `.symbols`
    /// baseline (sole case input carrying the stem, per the U2.8
    /// duplicate-stem disambiguation rule).
    #[must_use]
    pub fn owns_symbols_baseline(&self, input: &str) -> bool {
        let stem = case_stem(input);
        self.symbols_stems.contains(stem) && self.stem_case_counts.get(stem).copied() == Some(1)
    }
}

/// Render one interned type as its TypeScript `.types` display string.
///
/// Presentation lives harness-side over the public [`Type`] enum (per plan
/// §2.4): primitives lowercased, literals quoted/verbatim, `T[]` for arrays,
/// `A | B` for unions (grouped inside array/union context), best-effort
/// object and function shapes. `Error` renders as `any`, matching how tsc
/// displays the recovery type in `.types` baselines.
#[must_use]
pub fn render_type(model: &SemanticModel, type_id: TypeId) -> String {
    render_type_grouped(model, type_id, false)
}

fn render_type_grouped(model: &SemanticModel, type_id: TypeId, group: bool) -> String {
    match model.types().get(type_id) {
        Type::Error | Type::Any => "any".to_owned(),
        Type::Unknown => "unknown".to_owned(),
        Type::Never => "never".to_owned(),
        Type::Void => "void".to_owned(),
        Type::Null => "null".to_owned(),
        Type::Undefined => "undefined".to_owned(),
        Type::Boolean => "boolean".to_owned(),
        Type::Number => "number".to_owned(),
        Type::BigInt => "bigint".to_owned(),
        Type::String => "string".to_owned(),
        Type::Symbol => "symbol".to_owned(),
        Type::Object => "object".to_owned(),
        Type::BooleanLiteral(value) => if *value { "true" } else { "false" }.to_owned(),
        Type::NumberLiteral(text) | Type::BigIntLiteral(text) => text.to_string(),
        Type::StringLiteral(text) => render_string_literal(text),
        Type::Array(element) => format!("{}[]", render_type_grouped(model, *element, true)),
        Type::Tuple(shape) => {
            let mut elements = Vec::with_capacity(
                shape.prefix.len() + usize::from(shape.rest.is_some()) + shape.suffix.len(),
            );
            elements.extend(shape.prefix.iter().enumerate().map(|(index, element)| {
                let optional = index >= shape.required as usize;
                let rendered = render_type_grouped(model, *element, optional);
                if optional {
                    format!("{rendered}?")
                } else {
                    rendered
                }
            }));
            if let Some(rest) = shape.rest {
                elements.push(format!("...{}[]", render_type_grouped(model, rest, true)));
            }
            elements.extend(
                shape
                    .suffix
                    .iter()
                    .map(|element| render_type_grouped(model, *element, false)),
            );
            format!("[{}]", elements.join(", "))
        }
        Type::Union(members) => {
            let body = members
                .iter()
                .map(|member| render_type_grouped(model, *member, true))
                .collect::<Vec<_>>()
                .join(" | ");
            if group { format!("({body})") } else { body }
        }
        Type::Intersection(members) => {
            let body = members
                .iter()
                .map(|member| render_type_grouped(model, *member, true))
                .collect::<Vec<_>>()
                .join(" & ");
            if group { format!("({body})") } else { body }
        }
        Type::Function(signature) => {
            let type_params = if signature.type_parameters().is_empty() {
                String::new()
            } else {
                let names = signature
                    .type_parameters()
                    .iter()
                    .map(|symbol| model.symbol(*symbol).name().to_owned())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("<{names}>")
            };
            let params = signature
                .parameters()
                .iter()
                .map(|param| {
                    format!(
                        "{}: {}",
                        param.name(),
                        render_type_grouped(model, param.type_id(), false)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let body = format!(
                "{type_params}({params}) => {}",
                render_type_grouped(model, signature.return_type(), false)
            );
            if group { format!("({body})") } else { body }
        }
        Type::ObjectType(object) => {
            if object.properties().is_empty() {
                "{}".to_owned()
            } else {
                let body = object
                    .properties()
                    .iter()
                    .map(|property| {
                        format!(
                            "{}{}{}: {}",
                            if property.readonly() { "readonly " } else { "" },
                            property.name(),
                            if property.optional() { "?" } else { "" },
                            render_type_grouped(model, property.type_id(), false)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                format!("{{ {body}; }}")
            }
        }
        Type::AppliedClass { symbol, arguments } => {
            let name = model.symbol(*symbol).name();
            if arguments.is_empty() {
                name.to_owned()
            } else {
                let arguments = arguments
                    .iter()
                    .map(|argument| render_type_grouped(model, *argument, false))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{name}<{arguments}>")
            }
        }
        Type::Keyof(operand) => {
            let body = format!("keyof {}", render_type_grouped(model, *operand, true));
            if group { format!("({body})") } else { body }
        }
        Type::IndexedAccess { object, index } => format!(
            "{}[{}]",
            render_type_grouped(model, *object, true),
            render_type_grouped(model, *index, false)
        ),
        Type::Record { key, value } => format!(
            "Record<{}, {}>",
            render_type_grouped(model, *key, false),
            render_type_grouped(model, *value, false)
        ),
        Type::This { .. } => "this".to_owned(),
        Type::Named(symbol) | Type::NumericEnum(symbol) => model.symbol(*symbol).name().to_owned(),
    }
}

/// Renders a semantic string value as one canonical TypeScript literal.
fn render_string_literal(value: &EcmaString) -> String {
    let mut out = String::with_capacity(value.len_units() + 2);
    out.push('"');
    for (_, code_point) in value.code_points() {
        match char::from_u32(code_point) {
            Some('"') => out.push_str("\\\""),
            Some('\\') => out.push_str("\\\\"),
            Some('\n') => out.push_str("\\n"),
            Some('\t') => out.push_str("\\t"),
            Some('\r') => out.push_str("\\r"),
            Some('\u{0008}') => out.push_str("\\b"),
            Some('\u{000C}') => out.push_str("\\f"),
            Some('\u{000B}') => out.push_str("\\v"),
            Some(character) if character.is_control() && code_point < 0x20 => {
                write!(out, "\\u{code_point:04X}").expect("writing to String cannot fail");
            }
            Some('\u{2028}' | '\u{2029}') => {
                write!(out, "\\u{code_point:04X}").expect("writing to String cannot fail");
            }
            Some(character) => out.push(character),
            None => {
                write!(out, "\\u{code_point:04X}").expect("writing to String cannot fail");
            }
        }
    }
    out.push('"');
    out
}

/// One `>display : type` record plus its underline, anchored to a source line.
struct TypeAnnotation {
    start: usize,
    end: usize,
    line: usize,
    display: String,
    rendered: String,
}

/// Emit the `.types` baseline for a compiled case in the upstream framing:
/// a `//// [path] ////` marker, then one `=== <unit> ===` section per reached
/// unit whose source is echoed with `>expr : type` / `>  : ^^^` records under
/// each line. Only nodes the checker actually typed produce records, so the
/// document reproduces upstream exactly where the checker's coverage matches
/// and drops records where it does not (the S2 burn-down surface).
#[must_use]
pub fn emit_types_baseline(case: &CheckedCase, logical_path: &str) -> String {
    let mut out = format!("//// [{logical_path}] ////\n\n");
    for (unit, output) in case.reached_units() {
        let section = unit
            .virtual_path
            .rsplit('/')
            .next()
            .unwrap_or(&unit.virtual_path);
        emit_unit_types(
            output.semantic_model(),
            output.source_file().source_text(),
            section,
            &mut out,
        );
    }
    out
}

fn emit_unit_types(model: &SemanticModel, source: &SourceText, section: &str, out: &mut String) {
    let mut records: Vec<TypeAnnotation> = Vec::new();
    // Declaration-name records come from source-declared symbols (intrinsics
    // carry the default declaration node and an empty range).
    for (index, symbol) in model.symbols().iter().enumerate() {
        if symbol.declaration() == NodeId::default() {
            continue;
        }
        if symbol.kind() == bamts_compiler::checker::SymbolKind::TypeParameter {
            continue;
        }
        let range = symbol.range();
        if range.start() == range.end() {
            continue;
        }
        let type_id = model.symbol_type(SymbolId::new(index as u32));
        if let Some(annotation) = annotation_for(
            source,
            range,
            symbol.name().to_owned(),
            render_type(model, type_id),
        ) {
            records.push(annotation);
        }
    }
    // Expression records come from the checker's expression-type index.
    for (range, type_id) in model.typed_expressions() {
        let display = slice_source(source, *range);
        if let Some(annotation) =
            annotation_for(source, *range, display, render_type(model, *type_id))
        {
            records.push(annotation);
        }
    }
    // Upstream order: by start position, outer node before inner on ties.
    records.sort_by(|left, right| left.start.cmp(&right.start).then(right.end.cmp(&left.end)));
    let mut by_line: BTreeMap<usize, Vec<&TypeAnnotation>> = BTreeMap::new();
    for record in &records {
        by_line.entry(record.line).or_default().push(record);
    }

    out.push_str(&format!("=== {section} ===\n"));
    for (line_index, raw_line) in source.as_str().split('\n').enumerate() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        out.push_str(line);
        out.push('\n');
        let Some(line_records) = by_line.get(&line_index) else {
            continue;
        };
        for record in line_records {
            let display = collapse_whitespace(&record.display);
            out.push_str(&format!(">{display} : {}\n", record.rendered));
            out.push_str(&format!(
                ">{} : {}\n",
                " ".repeat(display.chars().count()),
                "^".repeat(record.rendered.chars().count())
            ));
        }
    }
}

/// Build an annotation record, resolving the node's start line. Records whose
/// range cannot be mapped (should not happen for compiled units) are dropped.
fn annotation_for(
    source: &SourceText,
    range: TextRange,
    display: String,
    rendered: String,
) -> Option<TypeAnnotation> {
    let (line, _) = source.line_column(range.start()).ok()?;
    Some(TypeAnnotation {
        start: range.start().get(),
        end: range.end().get(),
        line,
        display,
        rendered,
    })
}

/// Slice the raw source text covered by a UTF-16 range.
fn slice_source(source: &SourceText, range: TextRange) -> String {
    let text = source.as_str();
    match (
        source.utf16_to_byte(range.start()),
        source.utf16_to_byte(range.end()),
    ) {
        (Ok(start), Ok(end)) if start <= end && end <= text.len() => text[start..end].to_owned(),
        _ => String::new(),
    }
}

/// Collapse runs of whitespace (including newlines) to single spaces so a
/// multi-line node's echo stays on one record line; the comparator is
/// whitespace-insensitive on the record's left side.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The verdict of one types check cell.
#[derive(Debug, Clone)]
pub struct TypesOutcome {
    /// The emitted `.types` document.
    pub emitted: String,
    /// The comparator verdict against the owned baseline.
    pub verdict: FacetVerdict,
}

/// Resolve the `.types` baseline logical path owned by one case. Classification
/// guarantees sole-stem ownership for included rows, so resolution is by stem:
/// the plain (no-suffix) baseline wins, else the lexicographically first
/// variant (option semantics are unimplemented, so variants compile alike).
pub fn resolve_types_baseline(groups: &BaselineGroups, logical_path: &str) -> Option<String> {
    let stem = case_stem(logical_path);
    let candidates = groups.get(&(stem.to_owned(), "types".to_owned()))?;
    let plain = candidates.iter().find(|(suffix, _)| suffix.is_empty());
    let chosen = plain.or_else(|| candidates.iter().min_by(|left, right| left.0.cmp(&right.0)));
    chosen.map(|(_, path)| path.clone())
}

/// Compare one compiled case's emitted `.types` document against its baseline.
pub fn check_types(
    snapshot: &SuiteSnapshot,
    case: &CheckedCase,
    baseline_path: &str,
) -> TypesOutcome {
    let emitted = emit_types_baseline(case, &case.entry);
    let expected = snapshot
        .index
        .entries
        .get(baseline_path)
        .map(|entry| snapshot.root.join("baselines").join(&entry.sha256))
        .and_then(|blob| fs::read_to_string(blob).ok())
        .unwrap_or_default();
    let verdict = compare_types(&expected, &emitted);
    TypesOutcome { emitted, verdict }
}

/// Execute the S2 `types` observation for one planned cell.
///
/// Compile failures are `FAIL_BEHAVIOR` (loader/bound) or `CRASH` (panic);
/// comparator `Fail` is `FAIL_BEHAVIOR` (a checker-depth/format mismatch, the
/// S2 burn-down surface); `Unproven` (baseline won't canonicalize) is an
/// oracle-side `HARNESS_ERROR`; a missing owned baseline is a
/// classification/execution drift `HARNESS_ERROR`.
pub(crate) fn execute_types_check(
    snapshot: &SuiteSnapshot,
    groups: &BaselineGroups,
    plan: &PlannedCell,
    index_entry: &IndexEntry,
) -> Result<CellResult> {
    let blob = snapshot.root.join("cases").join(&index_entry.sha256);
    let bytes = fs::read(&blob).map_err(|error| {
        VerificationError::new(
            ErrorCode::Io,
            format!("cannot read case blob `{}`: {error}", blob.display()),
        )
    })?;
    let text = decode_case_source(&bytes);
    let pragmas = parse_case_pragmas(&text);
    let units = split_case_units(&index_entry.logical_path, &text);
    let entry = entry_virtual_path(&index_entry.logical_path, &units);
    let baseline_path = resolve_types_baseline(groups, &index_entry.logical_path);

    let (class, detail) = match compile_case_with_pragmas(&units, &entry, &pragmas) {
        Err(error) => {
            let detail = match &error {
                CaseCompileError::Load(detail)
                | CaseCompileError::SourceBound(detail)
                | CaseCompileError::Panic(detail) => detail.clone(),
            };
            (error.failure_class(), detail)
        }
        Ok(case) => {
            let Some(baseline_path) = baseline_path else {
                return Ok(CellResult {
                    entry_id: plan.entry.id.clone(),
                    facet: plan.entry.facet,
                    backend: plan.backend,
                    class: FailureClass::HarnessError,
                    detail: "classification/execution drift: no owned `.types` baseline".to_owned(),
                });
            };
            let outcome = check_types(snapshot, &case, &baseline_path);
            match outcome.verdict {
                FacetVerdict::Pass => (FailureClass::Pass, "types parity".to_owned()),
                FacetVerdict::Fail { reason } => (FailureClass::FailBehavior, reason),
                FacetVerdict::Unproven { reason } => (
                    FailureClass::HarnessError,
                    format!("types oracle could not prove parity: {reason}"),
                ),
            }
        }
    };
    Ok(CellResult {
        entry_id: plan.entry.id.clone(),
        facet: plan.entry.facet,
        backend: plan.backend,
        class,
        detail,
    })
}

/// Emit the `.symbols` baseline for a compiled case in the upstream framing:
/// a `//// [path] ////` marker, then one `=== <unit> ===` section per reached
/// unit whose source is echoed with `>name : Symbol(qname, Decl(unit, l, c))`
/// records under each line where a bound name is declared or referenced.
///
/// Coverage is bounded by what the binder tracks: declaration-name symbols and
/// resolved value/type references reproduce upstream records. Enum members
/// qualify against their enum at both declaration and initializer-reference
/// sites (`E.A`); class and interface member qualification lights up once
/// member binding lands (the scope-owner substrate is in place). Namespace
/// exports render bare at declarations per upstream, with access-path
/// qualification (`A.x`) pending member-access reference resolution. Class
/// members not yet bound as symbols, references to intrinsic/library symbols
/// (whose declaration lives outside the unit), and multi-`Decl` merged records
/// remain known gaps — the S2 burn-down surface, exactly like the `.types`
/// emitter.
#[must_use]
pub fn emit_symbols_baseline(case: &CheckedCase, logical_path: &str) -> String {
    let mut out = format!("//// [{logical_path}] ////\n\n");
    for (unit, output) in case.reached_units() {
        let section = unit
            .virtual_path
            .rsplit('/')
            .next()
            .unwrap_or(&unit.virtual_path);
        emit_unit_symbols(
            output.semantic_model(),
            output.source_file(),
            section,
            &mut out,
        );
    }
    out
}

/// One `>name : Symbol(...)` record anchored to a source line.
struct SymbolRecord {
    start: usize,
    end: usize,
    line: usize,
    display: String,
    rendered: String,
}

fn emit_unit_symbols(
    model: &SemanticModel,
    source_file: &bamts_compiler::syntax::SourceFile,
    section: &str,
    out: &mut String,
) {
    let source = source_file.source_text();
    let tokens = source_file.tokens();
    // The declaration position each symbol renders as `Decl(section, l, c)`.
    // `None` marks intrinsics and library symbols whose declaration is not in
    // this unit; occurrences resolving to them are dropped.
    let decl_positions: Vec<Option<(usize, usize)>> = model
        .symbols()
        .iter()
        .map(|symbol| symbol_decl_position(tokens, source, symbol))
        .collect();
    let render = |symbol_id: SymbolId| -> Option<String> {
        let index = symbol_id.get() as usize;
        let (line, character) = (*decl_positions.get(index)?)?;
        let name = model.qualified_name(symbol_id);
        Some(format!(
            "Symbol({name}, Decl({section}, {line}, {character}))"
        ))
    };

    let mut records: Vec<SymbolRecord> = Vec::new();
    // Declaration-name records: each source-declared symbol at its identifier.
    for (index, symbol) in model.symbols().iter().enumerate() {
        if symbol.declaration() == NodeId::default() || symbol.range().is_empty() {
            continue;
        }
        let symbol_id = SymbolId::new(index as u32);
        if let Some(rendered) = render(symbol_id)
            && let Some(record) =
                symbol_record_for(source, symbol.range(), symbol.name().to_owned(), rendered)
        {
            records.push(record);
        }
    }
    // Reference records: each resolved value/type occurrence at its use site.
    for (range, symbol_id) in model.symbol_references() {
        if let Some(rendered) = render(*symbol_id) {
            let display = collapse_whitespace(&slice_source(source, *range));
            if let Some(record) = symbol_record_for(source, *range, display, rendered) {
                records.push(record);
            }
        }
    }
    // Upstream order: by start position, outer node before inner on ties.
    records.sort_by(|left, right| left.start.cmp(&right.start).then(right.end.cmp(&left.end)));
    let mut by_line: BTreeMap<usize, Vec<&SymbolRecord>> = BTreeMap::new();
    for record in &records {
        by_line.entry(record.line).or_default().push(record);
    }

    out.push_str(&format!("=== {section} ===\n"));
    for (line_index, raw_line) in source.as_str().split('\n').enumerate() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        out.push_str(line);
        out.push('\n');
        let Some(line_records) = by_line.get(&line_index) else {
            continue;
        };
        for record in line_records {
            out.push_str(&format!(">{} : {}\n", record.display, record.rendered));
        }
    }
}

/// Build a symbol record, resolving the occurrence's start line.
fn symbol_record_for(
    source: &SourceText,
    range: TextRange,
    display: String,
    rendered: String,
) -> Option<SymbolRecord> {
    let (line, _) = source.line_column(range.start()).ok()?;
    Some(SymbolRecord {
        start: range.start().get(),
        end: range.end().get(),
        line,
        display,
        rendered,
    })
}

/// The 0-based `(line, character)` a symbol renders in its `Decl(...)` marker:
/// the declaration node's full start, i.e. the end of the significant token
/// immediately preceding the declaration (TypeScript's `node.pos`, which
/// counts leading trivia). The identifier token is located in the unit's
/// token stream; for keyword-led declarations (class/function/interface/type/
/// enum/namespace) the scan steps left over the leading keyword and modifiers
/// so the marker anchors at the declaration keyword, not the name.
fn symbol_decl_position(
    tokens: &[Token],
    source: &SourceText,
    symbol: &bamts_compiler::checker::Symbol,
) -> Option<(usize, usize)> {
    if symbol.declaration() == NodeId::default() || symbol.range().is_empty() {
        return None;
    }
    let id_start = symbol.range().start();
    let ident_index = tokens.iter().position(|token| {
        !token.is_missing() && !is_trivia_token(token.kind()) && token.range().start() == id_start
    })?;
    // Step the declaration's first token left over its leading keyword and
    // modifiers (skipping the trivia the parser retains between tokens), so a
    // keyword-led declaration anchors at its keyword rather than its name.
    let mut node_index = ident_index;
    if kind_is_keyword_led(symbol.kind()) {
        while let Some(prev) = prev_significant_token(tokens, node_index) {
            if is_declaration_leading(tokens[prev].kind()) {
                node_index = prev;
            } else {
                break;
            }
        }
    }
    // The declaration's full start is the end of the significant token that
    // precedes it (TypeScript's `node.pos`, which counts leading trivia).
    let full_start = match prev_significant_token(tokens, node_index) {
        Some(prev) => tokens[prev].range().end(),
        None => Utf16Pos::ZERO,
    };
    source.line_column(full_start).ok()
}

/// The index of the significant (non-trivia, non-missing) token before `index`.
fn prev_significant_token(tokens: &[Token], index: usize) -> Option<usize> {
    tokens[..index]
        .iter()
        .rposition(|token| !token.is_missing() && !is_trivia_token(token.kind()))
}

/// Whether a token is trivia the parser retains in the token stream.
const fn is_trivia_token(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Whitespace
            | TokenKind::LineComment
            | TokenKind::BlockComment
            | TokenKind::Shebang
    )
}

/// Declarations whose node begins with a leading keyword before the name.
const fn kind_is_keyword_led(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Function
            | SymbolKind::Interface
            | SymbolKind::TypeAlias
            | SymbolKind::Enum
            | SymbolKind::Namespace
    )
}

/// Tokens that can lead a keyword-led declaration (its keyword plus modifiers),
/// scanned over to anchor `Decl(...)` at the declaration's full start.
const fn is_declaration_leading(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::KwClass
            | TokenKind::KwFunction
            | TokenKind::KwInterface
            | TokenKind::KwType
            | TokenKind::KwEnum
            | TokenKind::KwNamespace
            | TokenKind::KwExport
            | TokenKind::KwDeclare
            | TokenKind::KwAbstract
            | TokenKind::KwDefault
            | TokenKind::KwConst
            | TokenKind::KwAsync
            | TokenKind::KwPublic
            | TokenKind::KwPrivate
            | TokenKind::KwProtected
            | TokenKind::KwStatic
            | TokenKind::KwReadonly
            | TokenKind::KwGet
            | TokenKind::KwSet
    )
}

/// The verdict of one symbols check cell.
#[derive(Debug, Clone)]
pub struct SymbolsOutcome {
    /// The emitted `.symbols` document.
    pub emitted: String,
    /// The comparator verdict against the owned baseline.
    pub verdict: FacetVerdict,
}

/// Resolve the `.symbols` baseline logical path owned by one case. As for
/// `.types`, classification guarantees sole-stem ownership for included rows,
/// so resolution is by stem: the plain (no-suffix) baseline wins, else the
/// lexicographically first variant.
pub fn resolve_symbols_baseline(groups: &BaselineGroups, logical_path: &str) -> Option<String> {
    let stem = case_stem(logical_path);
    let candidates = groups.get(&(stem.to_owned(), "symbols".to_owned()))?;
    let plain = candidates.iter().find(|(suffix, _)| suffix.is_empty());
    let chosen = plain.or_else(|| candidates.iter().min_by(|left, right| left.0.cmp(&right.0)));
    chosen.map(|(_, path)| path.clone())
}

/// Compare one compiled case's emitted `.symbols` document against its baseline.
pub fn check_symbols(
    snapshot: &SuiteSnapshot,
    case: &CheckedCase,
    baseline_path: &str,
) -> SymbolsOutcome {
    let emitted = emit_symbols_baseline(case, &case.entry);
    let expected = snapshot
        .index
        .entries
        .get(baseline_path)
        .map(|entry| snapshot.root.join("baselines").join(&entry.sha256))
        .and_then(|blob| fs::read_to_string(blob).ok())
        .unwrap_or_default();
    let verdict = compare_symbols(&expected, &emitted);
    SymbolsOutcome { emitted, verdict }
}

/// Execute the S2 `symbols` observation for one planned cell.
///
/// Failure-class discipline mirrors `execute_types_check`: compile failures
/// are `FAIL_BEHAVIOR` (loader/bound) or `CRASH` (panic); comparator `Fail`
/// is `FAIL_BEHAVIOR` (a checker-depth/format mismatch, the S2 burn-down
/// surface); `Unproven` (baseline won't canonicalize) is an oracle-side
/// `HARNESS_ERROR`; a missing owned baseline is a classification/execution
/// drift `HARNESS_ERROR`.
pub(crate) fn execute_symbols_check(
    snapshot: &SuiteSnapshot,
    groups: &BaselineGroups,
    plan: &PlannedCell,
    index_entry: &IndexEntry,
) -> Result<CellResult> {
    let blob = snapshot.root.join("cases").join(&index_entry.sha256);
    let bytes = fs::read(&blob).map_err(|error| {
        VerificationError::new(
            ErrorCode::Io,
            format!("cannot read case blob `{}`: {error}", blob.display()),
        )
    })?;
    let text = decode_case_source(&bytes);
    let pragmas = parse_case_pragmas(&text);
    let units = split_case_units(&index_entry.logical_path, &text);
    let entry = entry_virtual_path(&index_entry.logical_path, &units);
    let baseline_path = resolve_symbols_baseline(groups, &index_entry.logical_path);

    let (class, detail) = match compile_case_with_pragmas(&units, &entry, &pragmas) {
        Err(error) => {
            let detail = match &error {
                CaseCompileError::Load(detail)
                | CaseCompileError::SourceBound(detail)
                | CaseCompileError::Panic(detail) => detail.clone(),
            };
            (error.failure_class(), detail)
        }
        Ok(case) => {
            let Some(baseline_path) = baseline_path else {
                return Ok(CellResult {
                    entry_id: plan.entry.id.clone(),
                    facet: plan.entry.facet,
                    backend: plan.backend,
                    class: FailureClass::HarnessError,
                    detail: "classification/execution drift: no owned `.symbols` baseline"
                        .to_owned(),
                });
            };
            let outcome = check_symbols(snapshot, &case, &baseline_path);
            match outcome.verdict {
                FacetVerdict::Pass => (FailureClass::Pass, "symbols parity".to_owned()),
                FacetVerdict::Fail { reason } => (FailureClass::FailBehavior, reason),
                FacetVerdict::Unproven { reason } => (
                    FailureClass::HarnessError,
                    format!("symbols oracle could not prove parity: {reason}"),
                ),
            }
        }
    };
    Ok(CellResult {
        entry_id: plan.entry.id.clone(),
        facet: plan.entry.facet,
        backend: plan.backend,
        class,
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facets::load_diagnostic_code_map;

    fn repo_code_map() -> DiagnosticCodeMap {
        // `cargo test` runs with the package root as the working directory;
        // the code map is a committed oracle document two levels up.
        load_diagnostic_code_map(Path::new("../..")).expect("repository code map loads")
    }

    /// H1 end-to-end pin: a positive-diagnostics case compiles through the
    /// real loader+frontend, its lint warnings are filtered out, and the
    /// remaining checker diagnostic matches an upstream-position row
    /// (1-based line, 0-based character) under the code map.
    #[test]
    fn diagnostics_end_to_end_unresolved_name_passes() {
        let case_text = "var y = undefinedName;
";
        let units = split_case_units("tests/cases/compiler/endToEndPin.ts", case_text);
        let entry = entry_virtual_path("tests/cases/compiler/endToEndPin.ts", &units);
        let case = compile_case(&units, &entry).expect("case compiles");
        let code_map = repo_code_map();

        let mut actual = collect_facet_diagnostics(&case);
        actual.retain(|diagnostic| code_map.get(&diagnostic.code).is_some());
        // The lint profile's unused-local warning must not survive the filter.
        assert_eq!(actual.len(), 1, "only the checker diagnostic remains");
        assert_eq!(actual[0].code, "BAMTS-C002");
        assert_eq!(
            actual[0].position,
            SourcePosition {
                line: 1,
                character: 8
            }
        );

        // `undefinedName` starts at 0-based UTF-16 column 8 (TS prints 1:9).
        let expected = vec![FacetDiagnostic {
            unit: "endToEndPin.ts".to_owned(),
            position: SourcePosition {
                line: 1,
                character: 8,
            },
            category: DiagnosticCategory::Error,
            severity: FacetSeverity::Error,
            code: "TS2304".to_owned(),
        }];
        let verdict = compare_diagnostics(&expected, &actual, &code_map);
        assert_eq!(verdict, FacetVerdict::Pass);
    }

    /// Multi-unit cases with an empty leading unit must still load every file,
    /// so import conflicts in a dependent unit are observed.
    #[test]
    fn diagnostics_end_to_end_multi_unit_import_conflict_is_reached() {
        let case_text = "// @target: es2015\n\
            // @module: commonjs\n\
            // @filename: f1.ts\n\
            export function f() {\n\
            }\n\
            // @filename: f2.ts\n\
            import {f} from './f1';\n\
            export function f() {\n\
            }\n";
        let logical = "tests/cases/compiler/functionAndImportNameConflict.ts";
        let units = split_case_units(logical, case_text);
        let entry = entry_virtual_path(logical, &units);
        let case = compile_case(&units, &entry).expect("case compiles");
        let code_map = repo_code_map();

        let mut actual = collect_facet_diagnostics(&case);
        actual.retain(|diagnostic| code_map.get(&diagnostic.code).is_some());
        assert_eq!(
            actual.len(),
            1,
            "the import/local conflict in f2.ts must be emitted"
        );
        assert_eq!(actual[0].code, "BAMTS-C041");
        assert_eq!(
            actual[0].position,
            SourcePosition {
                line: 1,
                character: 8,
            }
        );

        let expected = vec![FacetDiagnostic {
            unit: "f2.ts".to_owned(),
            position: SourcePosition {
                line: 1,
                character: 8,
            },
            category: DiagnosticCategory::Error,
            severity: FacetSeverity::Error,
            code: "TS2440".to_owned(),
        }];
        let verdict = compare_diagnostics(&expected, &actual, &code_map);
        assert_eq!(verdict, FacetVerdict::Pass);
    }

    /// Zero-expectation pin: a clean case with no owned `.errors.txt` passes
    /// iff the compile emits no mapped diagnostics.
    #[test]
    fn diagnostics_end_to_end_clean_case_passes() {
        let case_text = "export const answer: number = 42;
";
        let units = split_case_units("tests/cases/compiler/cleanPin.ts", case_text);
        let entry = entry_virtual_path("tests/cases/compiler/cleanPin.ts", &units);
        let case = compile_case(&units, &entry).expect("case compiles");
        let code_map = repo_code_map();
        let mut actual = collect_facet_diagnostics(&case);
        actual.retain(|diagnostic| code_map.get(&diagnostic.code).is_some());
        let verdict = compare_diagnostics(&[], &actual, &code_map);
        assert_eq!(verdict, FacetVerdict::Pass);
    }

    /// Regression: two diagnostics at the same line/column in different files
    /// must not compare equal. Before the unit field was carried through
    /// `FacetDiagnostic`, the comparator keyed only on position/category/
    /// severity/code and a cross-unit collision silently passed.
    #[test]
    fn diagnostics_cross_unit_position_collision_is_detected() {
        let code_map = repo_code_map();
        let expected = vec![FacetDiagnostic {
            unit: "a.ts".to_owned(),
            position: SourcePosition {
                line: 3,
                character: 5,
            },
            category: DiagnosticCategory::Error,
            severity: FacetSeverity::Error,
            code: "TS2304".to_owned(),
        }];
        let actual = vec![FacetDiagnostic {
            unit: "b.ts".to_owned(),
            position: SourcePosition {
                line: 3,
                character: 5,
            },
            category: DiagnosticCategory::Error,
            severity: FacetSeverity::Error,
            code: "TS2304".to_owned(),
        }];
        let verdict = compare_diagnostics(&expected, &actual, &code_map);
        assert!(
            verdict.is_fail(),
            "same position in different files must fail, got {verdict:?}"
        );
        assert!(
            format!("{verdict:?}").contains("unit mismatch"),
            "failure must name the unit mismatch, got {verdict:?}"
        );
    }

    const ANY_AS_CONSTRUCTOR_CASE: &str = "\
// @target: es2015
// any is considered an untyped function call
// can be called except with type arguments which is an error

var x: any;
var a = new x();
var b = new x('hello');
var c = new x(x);

// grammar allows this for constructors
var d = new x<any>(x); // no error
";

    const ANY_AS_CONSTRUCTOR_ERRORS: &str = "\
anyAsConstructor.ts(10,9): error TS2347: Untyped function calls may not accept type arguments.


==== anyAsConstructor.ts (1 errors) ====
    // any is considered an untyped function call
    // can be called except with type arguments which is an error
    
    var x: any;
    var a = new x();
    var b = new x('hello');
    var c = new x(x);
    
    // grammar allows this for constructors
    var d = new x<any>(x); // no error
            ~~~~~~~~~~~~~
!!! error TS2347: Untyped function calls may not accept type arguments.
";

    const PARSER_ENUM_DECLARATION_2_ERRORS: &str = "\
parserEnumDeclaration2.ts(2,3): error TS1038: A 'declare' modifier cannot be used in an already ambient context.


==== parserEnumDeclaration2.ts (1 errors) ====
    declare namespace M {
      declare enum E {
      ~~~~~~~
!!! error TS1038: A 'declare' modifier cannot be used in an already ambient context.
      }
    }
";

    const MERGING1_ERRORS: &str = "\
testB.ts(1,22): error TS2305: Module '\"*.foo\"' has no exported member 'onlyInA'.


==== types.ts (0 errors) ====
    declare module \"*.foo\" {
      let everywhere: string;
    }
    
    
==== testA.ts (0 errors) ====
    import { everywhere, onlyInA } from \"a.foo\";
    declare module \"a.foo\" {
      let onlyInA: number;
    }
    
==== testB.ts (1 errors) ====
    import { everywhere, onlyInA } from \"b.foo\"; // Error
                         ~~~~~~~
!!! error TS2305: Module '\"*.foo\"' has no exported member 'onlyInA'.
    
";

    const PROJECT_BASELINE_AMD_ERRORS: &str = "\
error TS5107: Option 'module=AMD' is deprecated and will stop functioning in TypeScript 7.0. Specify compilerOption '\"ignoreDeprecations\": \"6.0\"' to silence this error.
error TS5107: Option 'moduleResolution=classic' is deprecated and will stop functioning in TypeScript 7.0. Specify compilerOption '\"ignoreDeprecations\": \"6.0\"' to silence this error.


!!! error TS5107: Option 'module=AMD' is deprecated and will stop functioning in TypeScript 7.0. Specify compilerOption '\"ignoreDeprecations\": \"6.0\"' to silence this error.
!!! error TS5107: Option 'moduleResolution=classic' is deprecated and will stop functioning in TypeScript 7.0. Specify compilerOption '\"ignoreDeprecations\": \"6.0\"' to silence this error.
==== decl.ts (0 errors) ====
    export interface Point { x: number; y: number; };
    export function point (x: number, y: number): Point {
    	return { x: x, y: y };
    }
==== emit.ts (0 errors) ====
    import g = require(\"./decl\");
    var p = g.point(10,20);
";

    #[test]
    fn parse_case_pragmas_reads_target_and_single_unit() {
        let pragmas = parse_case_pragmas(ANY_AS_CONSTRUCTOR_CASE);
        assert_eq!(
            pragmas.options,
            vec![("target".to_owned(), vec!["es2015".to_owned()])]
        );
        assert!(!pragmas.no_types_and_symbols);
    }

    #[test]
    fn parse_case_pragmas_lists_variants_in_order() {
        let pragmas =
            parse_case_pragmas("//@target: ES5, ES2015\n// @strict: true\nvar x: number;\n");
        assert_eq!(
            pragmas.options,
            vec![
                (
                    "target".to_owned(),
                    vec!["ES5".to_owned(), "ES2015".to_owned()]
                ),
                ("strict".to_owned(), vec!["true".to_owned()]),
            ]
        );
    }

    #[test]
    fn parse_case_pragmas_pins_no_types_and_symbols() {
        let pragmas = parse_case_pragmas("// @noTypesAndSymbols: true\nvar x: number;\n");
        assert!(pragmas.no_types_and_symbols);
        let pragmas = parse_case_pragmas("// @noTypesAndSymbols: false\nvar x: number;\n");
        assert!(!pragmas.no_types_and_symbols);
    }

    #[test]
    fn split_case_units_single_unit_case() {
        let units = split_case_units(
            "tests/cases/conformance/types/any/anyAsConstructor.ts",
            ANY_AS_CONSTRUCTOR_CASE,
        );
        assert_eq!(units.len(), 1);
        assert_eq!(
            units[0].virtual_path,
            "tests/cases/conformance/types/any/anyAsConstructor.ts"
        );
        // Upstream strips the `// @target` directive line from unit content, so
        // the compiled entry begins at the first real source line.
        assert!(!units[0].text.contains("@target"));
        assert_eq!(
            units[0].text,
            "// any is considered an untyped function call\n\
             // can be called except with type arguments which is an error\n\
             \n\
             var x: any;\n\
             var a = new x();\n\
             var b = new x('hello');\n\
             var c = new x(x);\n\
             \n\
             // grammar allows this for constructors\n\
             var d = new x<any>(x); // no error\n"
        );
    }

    #[test]
    fn split_case_units_strips_directives_and_renumbers() {
        // A `@target` directive plus a following blank line: upstream removes
        // the directive line and collapses the leading blank so the first
        // declaration lands on line 0, while interior blanks are preserved.
        let case = "// @strict: true\n\nclass Foo {}\n\nvar a = 0;\n";
        let units = split_case_units("tests/cases/compiler/renumber.ts", case);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].text, "class Foo {}\n\nvar a = 0;\n");
    }

    #[test]
    fn is_option_directive_line_matches_upstream_grammar() {
        assert!(is_option_directive_line("// @target: es2015"));
        assert!(is_option_directive_line("//@strict:true"));
        assert!(is_option_directive_line("// @filename: a.ts"));
        // `///` reference directives and colonless `@word` lines are content.
        assert!(!is_option_directive_line("/// <reference path=\"x\" />"));
        assert!(!is_option_directive_line("// @ts-ignore"));
        // A multi-word name is not `\\w+`, so it is not a directive.
        assert!(!is_option_directive_line("// @param x: number"));
        assert!(!is_option_directive_line("var a = 0;"));
    }

    #[test]
    fn split_case_units_splits_named_units() {
        let case = "\
// @module: commonjs
// @filename: types.ts
declare module \"*.foo\" {
  let everywhere: string;
}


// @filename: testA.ts
import { everywhere, onlyInA } from \"a.foo\";
declare module \"a.foo\" {
  let onlyInA: number;
}

// @filename: testB.ts
import { everywhere, onlyInA } from \"b.foo\"; // Error
";
        let units = split_case_units(
            "tests/cases/conformance/ambient/ambientDeclarationsPatterns_merging1.ts",
            case,
        );
        assert_eq!(units.len(), 4);
        assert_eq!(
            units[0].virtual_path,
            "tests/cases/conformance/ambient/ambientDeclarationsPatterns_merging1.ts"
        );
        // The `// @module` global directive is the only line before the first
        // `@filename`, so the entry unit's stripped content is empty.
        assert_eq!(units[0].text, "");
        assert_eq!(
            units[1].virtual_path,
            "tests/cases/conformance/ambient/types.ts"
        );
        // The `@filename` directive line is stripped; content begins at the
        // first real source line of the unit.
        assert!(units[1].text.starts_with("declare module \"*.foo\" {\n"));
        assert_eq!(
            units[2].virtual_path,
            "tests/cases/conformance/ambient/testA.ts"
        );
        assert_eq!(
            units[3].virtual_path,
            "tests/cases/conformance/ambient/testB.ts"
        );
    }

    #[test]
    fn split_case_units_leading_directive_naming_case_is_entry() {
        let case = "\
// @filename: anyAsConstructor.ts
var x: any;
";
        let units = split_case_units(
            "tests/cases/conformance/types/any/anyAsConstructor.ts",
            case,
        );
        assert_eq!(units.len(), 1);
        assert_eq!(
            units[0].virtual_path,
            "tests/cases/conformance/types/any/anyAsConstructor.ts"
        );
        assert!(units[0].text.contains("var x: any;"));
    }

    #[test]
    fn split_case_units_absolute_virtual_paths_remap_to_root() {
        let case = "\
// @filename: /ref.d.ts
declare var r: number;
// @filename: main.ts
r.toFixed();
";
        let units = split_case_units("tests/cases/compiler/usesRef.ts", case);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].virtual_path, "ref.d.ts");
        assert_eq!(units[1].virtual_path, "tests/cases/compiler/main.ts");
    }

    #[test]
    fn split_case_units_repeated_name_appends() {
        let case = "\
// @filename: a.ts
export const a = 1;
// @filename: b.ts
export const b = 2;
// @filename: a.ts
export const a2 = 3;
";
        let units = split_case_units("tests/cases/compiler/append.ts", case);
        assert_eq!(units.len(), 2);
        assert!(units[0].text.contains("a = 1"));
        assert!(units[0].text.contains("a2 = 3"));
    }

    #[test]
    fn split_baseline_file_name_variants() {
        assert_eq!(
            split_baseline_file_name("anyAsConstructor.errors.txt"),
            Some(("anyAsConstructor", "errors.txt", ""))
        );
        assert_eq!(
            split_baseline_file_name("ES5For-ofTypeCheck6(target=es5).errors.txt"),
            Some(("ES5For-ofTypeCheck6", "errors.txt", "(target=es5)"))
        );
        assert_eq!(
            split_baseline_file_name("parserEnumDeclaration2.d.types"),
            Some(("parserEnumDeclaration2.d", "types", ""))
        );
        assert_eq!(split_baseline_file_name("plain.js"), None);
    }

    #[test]
    fn case_stem_keeps_declaration_suffix() {
        assert_eq!(
            case_stem(
                "tests/cases/conformance/parser/ecmascript5/EnumDeclarations/parserEnumDeclaration2.d.ts"
            ),
            "parserEnumDeclaration2.d"
        );
        assert_eq!(
            case_stem("tests/cases/conformance/types/any/anyAsConstructor.ts"),
            "anyAsConstructor"
        );
    }

    #[test]
    fn parse_errors_baseline_reads_file_rows() {
        let baseline = parse_errors_baseline(ANY_AS_CONSTRUCTOR_ERRORS);
        assert_eq!(baseline.globals, vec![]);
        assert_eq!(
            baseline.diagnostics,
            vec![ErrorsDiagnostic {
                unit: "anyAsConstructor.ts".to_owned(),
                line: 10,
                character: 8,
                category: "error".to_owned(),
                code: "TS2347".to_owned(),
                message: "Untyped function calls may not accept type arguments.".to_owned(),
            }]
        );
    }

    #[test]
    fn parse_errors_baseline_skips_carets_and_summaries() {
        let baseline = parse_errors_baseline(PARSER_ENUM_DECLARATION_2_ERRORS);
        assert_eq!(baseline.file_diagnostic_count(), 1);
        let row = &baseline.diagnostics[0];
        assert_eq!(row.unit, "parserEnumDeclaration2.ts");
        assert_eq!((row.line, row.character), (2, 2));
        assert_eq!(row.code, "TS1038");
        assert!(baseline.globals.is_empty());
    }

    #[test]
    fn parse_errors_baseline_multi_unit_rows() {
        let baseline = parse_errors_baseline(MERGING1_ERRORS);
        assert_eq!(baseline.file_diagnostic_count(), 1);
        assert_eq!(baseline.diagnostics[0].unit, "testB.ts");
        assert_eq!(
            (
                baseline.diagnostics[0].line,
                baseline.diagnostics[0].character
            ),
            (1, 21)
        );
        assert_eq!(baseline.diagnostics[0].code, "TS2305");
    }

    #[test]
    fn parse_errors_baseline_global_rows_are_separate() {
        let baseline = parse_errors_baseline(PROJECT_BASELINE_AMD_ERRORS);
        assert_eq!(baseline.file_diagnostic_count(), 0);
        assert_eq!(baseline.globals.len(), 2);
        assert!(
            baseline
                .globals
                .iter()
                .all(|global| global.code == "TS5107")
        );
        assert!(baseline.globals[0].message.contains("module=AMD"));
    }

    #[test]
    fn parse_errors_baseline_empty_document() {
        let baseline = parse_errors_baseline("");
        assert_eq!(baseline, ErrorsBaseline::default());
    }

    /// Compile a single-unit case and run `f` with the entry unit's model.
    fn with_model<R>(case_text: &str, logical: &str, f: impl FnOnce(&SemanticModel) -> R) -> R {
        let units = split_case_units(logical, case_text);
        let entry = entry_virtual_path(logical, &units);
        let case = compile_case(&units, &entry).expect("case compiles");
        let (_, output) = case
            .reached_units()
            .next()
            .expect("the entry unit is reached");
        f(output.semantic_model())
    }

    #[test]
    fn render_type_covers_primitives() {
        with_model("var x: any;\n", "tests/cases/compiler/p.ts", |model| {
            let table = model.types();
            assert_eq!(render_type(model, table.any()), "any");
            assert_eq!(render_type(model, table.unknown()), "unknown");
            assert_eq!(render_type(model, table.never()), "never");
            assert_eq!(render_type(model, table.void()), "void");
            assert_eq!(render_type(model, table.null_type()), "null");
            assert_eq!(render_type(model, table.undefined_type()), "undefined");
            assert_eq!(render_type(model, table.boolean()), "boolean");
            assert_eq!(render_type(model, table.number()), "number");
            assert_eq!(render_type(model, table.bigint()), "bigint");
            assert_eq!(render_type(model, table.string()), "string");
            assert_eq!(render_type(model, table.symbol_type()), "symbol");
            assert_eq!(render_type(model, table.object()), "object");
        });
    }

    #[test]
    fn render_type_covers_literals_and_arrays() {
        // The initializer expressions carry literal and array types the walk
        // recorded; render them straight from the model.
        with_model(
            "var s: string = \"hi\";\nvar n = 42;\nvar b = true;\nvar xs = [1];\n",
            "tests/cases/compiler/lit.ts",
            |model| {
                let rendered: Vec<String> = model
                    .typed_expressions()
                    .iter()
                    .map(|(_, type_id)| render_type(model, *type_id))
                    .collect();
                assert!(rendered.contains(&"\"hi\"".to_owned()), "{rendered:?}");
                assert!(rendered.contains(&"42".to_owned()), "{rendered:?}");
                assert!(rendered.contains(&"true".to_owned()), "{rendered:?}");
                // `[1]` types as an array over the `1` literal element.
                assert!(rendered.contains(&"1[]".to_owned()), "{rendered:?}");
            },
        );
    }

    #[test]
    fn renders_canonical_string_literals_without_losing_surrogates() {
        assert_eq!(
            render_string_literal(&EcmaString::encode("a\n\"b")),
            "\"a\\n\\\"b\""
        );
        assert_eq!(
            render_string_literal(&EcmaString::from_units(&[0xD800])),
            "\"\\uD800\""
        );
    }

    #[test]
    fn render_type_covers_tuple_shapes_and_applied_classes() {
        with_model(
            "declare class Box<T> {}\
             declare let optionalTuple: [number, string?];\
             declare let variadicTuple: [number, ...boolean[], bigint];\
             declare let box: Box<string>;",
            "tests/cases/compiler/composite.ts",
            |model| {
                let scope = model.scope(model.module_scope());
                let optional = scope
                    .value("optionalTuple")
                    .expect("optional tuple binding");
                assert_eq!(
                    render_type(model, model.symbol_type(optional)),
                    "[number, string?]"
                );
                let variadic = scope
                    .value("variadicTuple")
                    .expect("variadic tuple binding");
                assert_eq!(
                    render_type(model, model.symbol_type(variadic)),
                    "[number, ...boolean[], bigint]"
                );
                let box_value = scope.value("box").expect("box binding");
                assert_eq!(
                    render_type(model, model.symbol_type(box_value)),
                    "Box<string>"
                );
            },
        );
    }

    #[test]
    fn render_type_preserves_polymorphic_this_identity() {
        with_model(
            "class Base {
                 value = 1;
                 method(): number { return this.value; }
             }",
            "tests/cases/compiler/polymorphicThis.ts",
            |model| {
                let this_type = model
                    .typed_expressions()
                    .iter()
                    .map(|(_, type_id)| *type_id)
                    .find(|type_id| matches!(model.types().get(*type_id), Type::This { .. }))
                    .expect("method body has polymorphic this type");

                assert_eq!(render_type(model, this_type), "this");
            },
        );
    }

    /// H4 pin: an annotated scalar case the checker fully types reproduces its
    /// upstream `.types` baseline (marker, section, source echo, `>expr : type`
    /// records, and underline widths) under `compare_types`.
    #[test]
    fn emit_types_baseline_matches_fully_typed_scalar_case() {
        let logical = "tests/cases/compiler/scalarPin.ts";
        let case_text = "var x: any;\nvar s: string = \"hi\";\n";
        let units = split_case_units(logical, case_text);
        let entry = entry_virtual_path(logical, &units);
        let case = compile_case(&units, &entry).expect("case compiles");
        let emitted = emit_types_baseline(&case, logical);
        let baseline = "//// [tests/cases/compiler/scalarPin.ts] ////\n\
            \n\
            === scalarPin.ts ===\n\
            var x: any;\n\
            >x : any\n\
            >  : ^^^\n\
            \n\
            var s: string = \"hi\";\n\
            >s : string\n\
            >  : ^^^^^^\n\
            >\"hi\" : \"hi\"\n\
            >     : ^^^^\n";
        assert_eq!(
            compare_types(baseline, &emitted),
            FacetVerdict::Pass,
            "emitted:\n{emitted}"
        );
    }

    /// Honesty pin: where the checker types fewer nodes than tsc (here, the
    /// `new`/callee/argument sub-expressions of `anyAsConstructor`), the
    /// emitter drops those records and the comparator reports a `Fail` — the
    /// S2 burn-down surface, not a false pass.
    #[test]
    fn emit_types_baseline_undercoverage_fails_comparison() {
        let logical = "tests/cases/conformance/types/any/anyAsConstructor.ts";
        let units = split_case_units(logical, ANY_AS_CONSTRUCTOR_CASE);
        let entry = entry_virtual_path(logical, &units);
        let case = compile_case(&units, &entry).expect("case compiles");
        let emitted = emit_types_baseline(&case, logical);
        let baseline = "//// [tests/cases/conformance/types/any/anyAsConstructor.ts] ////\n\
            \n\
            === anyAsConstructor.ts ===\n\
            var x: any;\n\
            >x : any\n\
            >  : ^^^\n\
            \n\
            var a = new x();\n\
            >a : any\n\
            >  : ^^^\n\
            >new x() : any\n\
            >        : ^^^\n\
            >x : any\n\
            >  : ^^^\n";
        assert!(
            matches!(compare_types(baseline, &emitted), FacetVerdict::Fail { .. }),
            "under-covered emit must not pass; emitted:\n{emitted}"
        );
    }

    #[test]
    fn resolve_types_baseline_prefers_plain_over_variant() {
        let mut groups: BaselineGroups = BTreeMap::new();
        groups.insert(
            ("foo".to_owned(), "types".to_owned()),
            vec![
                (
                    "(target=es5)".to_owned(),
                    "tests/baselines/reference/foo(target=es5).types".to_owned(),
                ),
                (
                    String::new(),
                    "tests/baselines/reference/foo.types".to_owned(),
                ),
            ],
        );
        assert_eq!(
            resolve_types_baseline(&groups, "tests/cases/compiler/foo.ts").as_deref(),
            Some("tests/baselines/reference/foo.types")
        );
        assert_eq!(
            resolve_types_baseline(&groups, "tests/cases/compiler/absent.ts"),
            None
        );
    }

    /// End-to-end pin: a top-level case whose declarations and value reference
    /// the binder fully tracks reproduces the upstream `.symbols` framing —
    /// marker, section, source echo, and `>name : Symbol(name, Decl(unit, l, c))`
    /// records with full-start declaration positions — under `compare_symbols`.
    #[test]
    fn emit_symbols_baseline_reproduces_declaration_and_reference_records() {
        let logical = "tests/cases/compiler/symbolPin.ts";
        let case_text = "class Cell {}\nvar a = 0;\nvar b = a;\n";
        let units = split_case_units(logical, case_text);
        let entry = entry_virtual_path(logical, &units);
        let case = compile_case(&units, &entry).expect("case compiles");
        let emitted = emit_symbols_baseline(&case, logical);
        let baseline = "//// [tests/cases/compiler/symbolPin.ts] ////\n\
            \n\
            === symbolPin.ts ===\n\
            class Cell {}\n\
            >Cell : Symbol(Cell, Decl(symbolPin.ts, 0, 0))\n\
            var a = 0;\n\
            >a : Symbol(a, Decl(symbolPin.ts, 1, 3))\n\
            var b = a;\n\
            >b : Symbol(b, Decl(symbolPin.ts, 2, 3))\n\
            >a : Symbol(a, Decl(symbolPin.ts, 1, 3))\n";
        assert_eq!(
            compare_symbols(baseline, &emitted),
            FacetVerdict::Pass,
            "emitted:\n{emitted}"
        );
    }

    /// Honesty pin on a real pinned sample: `anyAsConstructor` carries a
    /// `@target` pragma (dropped from the upstream echo) and references whose
    /// declarations the binder anchors differently, so the emitted document
    /// cannot canonicalize equal to the upstream `.symbols` baseline — the S2
    /// burn-down surface, reported as `Fail`, never a false pass.
    #[test]
    fn emit_symbols_baseline_undercoverage_fails_comparison() {
        let logical = "tests/cases/conformance/types/any/anyAsConstructor.ts";
        let units = split_case_units(logical, ANY_AS_CONSTRUCTOR_CASE);
        let entry = entry_virtual_path(logical, &units);
        let case = compile_case(&units, &entry).expect("case compiles");
        let emitted = emit_symbols_baseline(&case, logical);
        let baseline = "//// [tests/cases/conformance/types/any/anyAsConstructor.ts] ////\n\
            \n\
            === anyAsConstructor.ts ===\n\
            var x: any;\n\
            >x : Symbol(x, Decl(anyAsConstructor.ts, 3, 3))\n\
            \n\
            var a = new x();\n\
            >a : Symbol(a, Decl(anyAsConstructor.ts, 4, 3))\n\
            >x : Symbol(x, Decl(anyAsConstructor.ts, 3, 3))\n";
        assert!(
            matches!(
                compare_symbols(baseline, &emitted),
                FacetVerdict::Fail { .. }
            ),
            "under-covered emit must not pass; emitted:\n{emitted}"
        );
    }

    /// Enum members qualify against their enum symbol at both the declaration
    /// site and the initializer-reference site (`E.A`), while the enum itself
    /// and an unrelated top-level `var` stay bare.
    #[test]
    fn emit_symbols_baseline_qualifies_enum_members() {
        let logical = "tests/cases/compiler/pin.ts";
        let case_text = "enum E {\n    A,\n    B = A + 1,\n}\nvar x = 1;\n";
        let units = split_case_units(logical, case_text);
        let entry = entry_virtual_path(logical, &units);
        let case = compile_case(&units, &entry).expect("case compiles");
        let emitted = emit_symbols_baseline(&case, logical);
        let baseline = "//// [tests/cases/compiler/pin.ts] ////\n\
            \n\
            === pin.ts ===\n\
            enum E {\n\
            >E : Symbol(E, Decl(pin.ts, 0, 0))\n\
            \n\
                A,\n\
            >A : Symbol(E.A, Decl(pin.ts, 0, 8))\n\
            \n\
                B = A + 1,\n\
            >B : Symbol(E.B, Decl(pin.ts, 1, 6))\n\
            >A : Symbol(E.A, Decl(pin.ts, 0, 8))\n\
            \n\
            }\n\
            var x = 1;\n\
            >x : Symbol(x, Decl(pin.ts, 4, 3))\n";
        assert_eq!(
            compare_symbols(baseline, &emitted),
            FacetVerdict::Pass,
            "emitted:\n{emitted}"
        );
    }

    /// Namespace export declarations render bare (`Symbol(x, ...)`), guarding
    /// against declare-time over-qualification: upstream qualifies namespace
    /// members only at access-path reference sites, which this emitter does not
    /// yet reach.
    #[test]
    fn emit_symbols_baseline_keeps_namespace_exports_bare() {
        let logical = "tests/cases/compiler/pin.ts";
        let case_text = "namespace A {\n    export var x = 1;\n}\n";
        let units = split_case_units(logical, case_text);
        let entry = entry_virtual_path(logical, &units);
        let case = compile_case(&units, &entry).expect("case compiles");
        let emitted = emit_symbols_baseline(&case, logical);
        let baseline = "//// [tests/cases/compiler/pin.ts] ////\n\
            \n\
            === pin.ts ===\n\
            namespace A {\n\
            >A : Symbol(A, Decl(pin.ts, 0, 0))\n\
            \n\
                export var x = 1;\n\
            >x : Symbol(x, Decl(pin.ts, 1, 14))\n\
            \n\
            }\n";
        assert_eq!(
            compare_symbols(baseline, &emitted),
            FacetVerdict::Pass,
            "emitted:\n{emitted}"
        );
    }

    #[test]
    fn resolve_symbols_baseline_prefers_plain_over_variant() {
        let mut groups: BaselineGroups = BTreeMap::new();
        groups.insert(
            ("foo".to_owned(), "symbols".to_owned()),
            vec![
                (
                    "(target=es5)".to_owned(),
                    "tests/baselines/reference/foo(target=es5).symbols".to_owned(),
                ),
                (
                    String::new(),
                    "tests/baselines/reference/foo.symbols".to_owned(),
                ),
            ],
        );
        assert_eq!(
            resolve_symbols_baseline(&groups, "tests/cases/compiler/foo.ts").as_deref(),
            Some("tests/baselines/reference/foo.symbols")
        );
        assert_eq!(
            resolve_symbols_baseline(&groups, "tests/cases/compiler/absent.ts"),
            None
        );
    }

    #[test]
    fn class_method_this_preserves_polymorphic_identity() {
        let logical = "tests/cases/compiler/thisPin.ts";
        let case_text = "class Ship { isSunk: boolean = false; }\n\
class Board {\n\
    ships: Ship[] = [];\n\
    private allShipsSunk() {\n\
        return this.ships.every(function (val) { return val.isSunk; });\n\
    }\n\
}\n";
        let units = split_case_units(logical, case_text);
        let entry = entry_virtual_path(logical, &units);
        let case = compile_case(&units, &entry).expect("case compiles");
        let baseline = emit_types_baseline(&case, logical);
        assert!(
            baseline.contains(">this.ships : Ship[]"),
            "this.ships should be Ship[]\n{baseline}"
        );
        assert!(
            baseline.contains(">this : this"),
            "this should preserve its polymorphic identity\n{baseline}"
        );
    }

    #[test]
    fn infer_rest_argument_type_parameter_from_multiple_arguments() {
        let source = "// @strict: true\n// @target: es2015\n\n// Repro from #31204\n\nexport enum AppType {\n    HeaderDetail = 'HeaderDetail',\n    HeaderMultiDetail = 'HeaderMultiDetail',\n    AdvancedList = 'AdvancedList',\n    Standard = 'Standard',\n    Relationship = 'Relationship',\n    Report = 'Report',\n    Composite = 'Composite',\n    ListOnly = 'ListOnly',\n    ModuleSettings = 'ModuleSettings'\n}\n\nexport enum AppStyle {\n    Tree,\n    TreeEntity,\n    Standard,\n    MiniApp,\n    PivotTable\n}\n\nconst appTypeStylesWithError: Map<AppType, Array<AppStyle>> = new Map([\n    [AppType.Standard, [AppStyle.Standard, AppStyle.MiniApp]],\n    [AppType.Relationship, [AppStyle.Standard, AppStyle.Tree, AppStyle.TreeEntity]],\n    [AppType.AdvancedList, [AppStyle.Standard, AppStyle.MiniApp]]\n]);\n\n// Repro from #31204\n\ndeclare function foo<T>(...args: T[]): T[];\nlet b1: { x: boolean }[] = foo({ x: true }, { x: false });\nlet b2: boolean[][] = foo([true], [false]);\n";
        let logical = "tests/cases/conformance/expressions/arrayLiterals/arrayLiteralInference.ts";
        let units = split_case_units(logical, source);
        let entry = entry_virtual_path(logical, &units);
        let pragmas = parse_case_pragmas(source);
        let case = compile_case_with_pragmas(&units, &entry, &pragmas).expect("compiles");
        let code_map = load_diagnostic_code_map(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap(),
        )
        .unwrap();
        let mut actual = collect_facet_diagnostics(&case);
        actual.retain(|diagnostic| code_map.get(&diagnostic.code).is_some());
        assert!(
            actual.is_empty(),
            "unexpected mapped diagnostics: {actual:?}"
        );
    }

    #[test]
    fn object_literal_getter_without_return_has_void_readonly_property() {
        let logical = "tests/cases/conformance/parser/ecmascript5/Accessors/parserAccessors7.ts";
        let source = "// @target: es5\nvar v = { get foo(v: number) { } };\n";
        let units = split_case_units(logical, source);
        let entry = entry_virtual_path(logical, &units);
        let case = compile_case(&units, &entry).expect("compiles");
        let baseline = emit_types_baseline(&case, logical);
        assert!(
            baseline.contains(">v : { readonly foo: void; }"),
            "getter should be readonly void property\n{baseline}"
        );
    }

    #[test]
    fn this_type_in_accessors_negative_object_type_is_widened_and_non_readonly() {
        let source = "// @noImplicitAny: true\n// @noImplicitThis: true\n// @target: es5, es2015\ninterface Foo {\n    n: number;\n    x: number;\n}\ninterface Bar {\n    wrong: \"place\" | \"time\" | \"method\" | \"technique\";\n}\nconst mismatch = {\n    n: 13,\n    get x(this: Foo) { return this.n; },\n    set x(this: Bar, n) { this.wrong = \"method\"; }\n}\nconst contextual: Foo = {\n    n: 16,\n    get x() { return this.n; }\n}\n";
        let logical = "tests/cases/conformance/types/thisType/thisTypeInAccessorsNegative.ts";
        let units = split_case_units(logical, source);
        let entry = entry_virtual_path(logical, &units);
        let pragmas = parse_case_pragmas(source);
        let case = compile_case_with_pragmas(&units, &entry, &pragmas).expect("compiles");
        let baseline = emit_types_baseline(&case, logical);
        assert!(
            baseline.contains(">mismatch : { n: number; x: number; }"),
            "mismatch object type should widen both properties to number and merge get/set\n{baseline}"
        );
    }
}
