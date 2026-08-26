//! tsc harness directives: `// @flags`, `// @option`, and `@filename` splits.
//!
//! A TypeScript compiler test may pack several virtual files and a set of
//! compiler flags into one source. This module is the compiler-side parser for
//! that surface: it yields confined virtual files (with original UTF-16
//! offsets) and typed options the driver and lane can consume. Invalid paths,
//! duplicate names, and malformed flags become diagnostics; a product is still
//! retained so recovery stays total.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use crate::diagnostic::{Diagnostic, DiagnosticCode, Recovered};
use crate::source::{JsxEmit, ScriptKind, SourceId, TextRange, Utf16Pos};

/// Unknown `// @name` directive.
const UNKNOWN_DIRECTIVE: DiagnosticCode = DiagnosticCode::new("BAMTS-D001");
/// `@filename` is missing a path.
const EMPTY_VIRTUAL_PATH: DiagnosticCode = DiagnosticCode::new("BAMTS-D002");
/// Two `@filename` directives name the same virtual path.
const DUPLICATE_VIRTUAL_PATH: DiagnosticCode = DiagnosticCode::new("BAMTS-D003");
/// A virtual path is absolute, contains `..`, or is otherwise unconfined.
const PATH_ESCAPES_PROJECT: DiagnosticCode = DiagnosticCode::new("BAMTS-D004");
/// A known option carries a value its closed set rejects.
const INVALID_OPTION_VALUE: DiagnosticCode = DiagnosticCode::new("BAMTS-D005");
/// Non-comment content appears before the first `@filename`.
const CONTENT_BEFORE_FILENAME: DiagnosticCode = DiagnosticCode::new("BAMTS-D006");
/// `@flags` is not a well-formed CLI flag list.
const MALFORMED_FLAGS: DiagnosticCode = DiagnosticCode::new("BAMTS-D007");

const SOURCE: SourceId = SourceId::new(0);

/// One virtual file extracted from a tsc test source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VirtualFile {
    path: String,
    text: String,
    origin_start: Utf16Pos,
    origin_byte: usize,
    script_kind: ScriptKind,
    options: DirectiveOptions,
}

impl VirtualFile {
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// UTF-16 start of the first retained body character in the original source.
    #[must_use]
    pub const fn origin_start(&self) -> Utf16Pos {
        self.origin_start
    }

    /// UTF-8 byte start of the first retained body character in the original source.
    #[must_use]
    pub const fn origin_byte(&self) -> usize {
        self.origin_byte
    }

    #[must_use]
    pub const fn script_kind(&self) -> ScriptKind {
        self.script_kind
    }

    /// Options declared after this file's `@filename` directive.
    #[must_use]
    pub const fn options(&self) -> &DirectiveOptions {
        &self.options
    }
}

/// Compiler options collected from `// @name: value` and `// @flags`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DirectiveOptions {
    values: BTreeMap<String, String>,
}

impl DirectiveOptions {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .get(&canonical_option_name(name))
            .map(String::as_str)
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    #[must_use]
    pub fn strict(&self) -> Option<bool> {
        parse_bool_value(self.get("strict")?)
    }

    #[must_use]
    pub fn target(&self) -> Option<&str> {
        self.get("target")
    }

    #[must_use]
    pub fn module(&self) -> Option<&str> {
        self.get("module")
    }

    #[must_use]
    pub fn jsx(&self) -> Option<JsxEmit> {
        JsxEmit::parse(self.get("jsx")?)
    }

    #[must_use]
    pub fn jsx_factory(&self) -> Option<&str> {
        self.get("jsxfactory")
    }

    #[must_use]
    pub fn jsx_fragment_factory(&self) -> Option<&str> {
        self.get("jsxfragmentfactory")
    }

    #[must_use]
    pub fn jsx_import_source(&self, script_kind: ScriptKind) -> Option<&str> {
        match script_kind {
            ScriptKind::TypeScriptReact => self
                .get("tsximportsource")
                .or_else(|| self.get("jsximportsource")),
            ScriptKind::JavaScriptReact => self.get("jsximportsource"),
            ScriptKind::JavaScript | ScriptKind::TypeScript | ScriptKind::Json => None,
        }
    }

    #[must_use]
    pub fn module_resolution(&self) -> Option<&str> {
        self.get("moduleresolution")
    }

    /// CLI-shaped flags (`--strict`, `--target esnext`) for the driver.
    #[must_use]
    pub fn to_cli_flags(&self) -> Vec<String> {
        let mut flags = Vec::new();
        for (name, value) in &self.values {
            if value.eq_ignore_ascii_case("true") {
                flags.push(format!("--{name}"));
            } else if value.eq_ignore_ascii_case("false") {
                flags.push(format!("--no-{name}"));
            } else {
                flags.push(format!("--{name}"));
                flags.push(value.clone());
            }
        }
        flags
    }

    fn insert(&mut self, name: String, value: String) {
        self.values.insert(name, value);
    }

    fn overlay(&self, overrides: &Self) -> Self {
        let mut values = self.values.clone();
        values.extend(overrides.values.clone());
        Self { values }
    }
}

/// Recovered virtual files plus the options that apply to every file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedDirectives {
    files: Vec<VirtualFile>,
    options: DirectiveOptions,
}

impl ParsedDirectives {
    #[must_use]
    pub fn files(&self) -> &[VirtualFile] {
        &self.files
    }

    #[must_use]
    pub fn options(&self) -> &DirectiveOptions {
        &self.options
    }

    /// Resolves global directives with declarations attached to `file` taking precedence.
    #[must_use]
    pub fn options_for(&self, file: &VirtualFile) -> DirectiveOptions {
        self.options.overlay(file.options())
    }

    /// Resolves a file directive over the project JSX mode.
    #[must_use]
    pub fn jsx_for(&self, file: &VirtualFile, project: Option<JsxEmit>) -> Option<JsxEmit> {
        file.options()
            .jsx()
            .or_else(|| self.options.jsx())
            .or(project)
    }

    #[must_use]
    pub fn jsx_factory_for<'a>(
        &'a self,
        file: &'a VirtualFile,
        project: Option<&'a str>,
    ) -> Option<&'a str> {
        file.options()
            .jsx_factory()
            .or_else(|| self.options.jsx_factory())
            .or(project)
    }

    #[must_use]
    pub fn jsx_fragment_factory_for<'a>(
        &'a self,
        file: &'a VirtualFile,
        project: Option<&'a str>,
    ) -> Option<&'a str> {
        file.options()
            .jsx_fragment_factory()
            .or_else(|| self.options.jsx_fragment_factory())
            .or(project)
    }

    #[must_use]
    pub fn jsx_import_source_for<'a>(
        &'a self,
        file: &'a VirtualFile,
        project: Option<&'a str>,
    ) -> Option<&'a str> {
        file.options()
            .jsx_import_source(file.script_kind())
            .or_else(|| self.options.jsx_import_source(file.script_kind()))
            .or(project)
    }
}

/// Parses `// @flags`, `// @option: value`, and `@filename` virtual splits.
///
/// `origin_name` is the confined path used when the source has no `@filename`.
#[must_use]
pub fn parse_tsc_directives(source: &str, origin_name: &str) -> Recovered<ParsedDirectives> {
    let mut diagnostics = Vec::new();
    let mut options = DirectiveOptions::default();
    let mut files = Vec::new();
    let mut seen_paths = BTreeSet::new();
    let mut current: Option<OpenFile> = None;
    let mut leading_body = String::new();
    let mut saw_filename = false;
    let mut byte = 0usize;
    let mut utf16 = 0usize;

    let mut rest = source;
    while !rest.is_empty() {
        let nl = rest.find('\n');
        let (raw_line, has_nl) = match nl {
            Some(index) => (&rest[..index], true),
            None => (rest, false),
        };
        let stripped = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let line_start_utf16 = utf16;
        let line_start_byte = byte;
        let range = line_range(line_start_utf16, raw_line);

        let directive_line = if byte == 0 {
            stripped.strip_prefix('\u{feff}').unwrap_or(stripped)
        } else {
            stripped
        };
        if let Some(directive) = parse_directive_line(directive_line) {
            match directive.name.as_str() {
                "filename" => {
                    if let Some(open) = current.take() {
                        files.push(open.into_file());
                    } else if saw_filename {
                        // The previous `@filename` was rejected; drop its body.
                    } else if !is_ws_only(&leading_body) {
                        diagnostics.push(error(
                            CONTENT_BEFORE_FILENAME,
                            range,
                            "non-comment content appears before the first @filename directive",
                        ));
                    }
                    saw_filename = true;
                    leading_body.clear();
                    match accept_filename(
                        &directive.value,
                        range,
                        &mut seen_paths,
                        &mut diagnostics,
                    ) {
                        Some(path) => {
                            let origin_start = if has_nl {
                                Utf16Pos::new(utf16 + utf16_len(raw_line) + 1)
                            } else {
                                Utf16Pos::new(utf16 + utf16_len(raw_line))
                            };
                            let origin_byte = if has_nl {
                                byte + raw_line.len() + 1
                            } else {
                                byte + raw_line.len()
                            };
                            current = Some(OpenFile {
                                path,
                                body: String::new(),
                                origin_start,
                                origin_byte,
                                options: DirectiveOptions::default(),
                            });
                        }
                        None => current = None,
                    }
                }
                "flags" => apply_flags(&directive.value, range, &mut options, &mut diagnostics),
                "link" => apply_link(&directive.value, range, &mut diagnostics),
                name => {
                    let destination = current
                        .as_mut()
                        .filter(|_| is_per_file_option(name))
                        .map_or(&mut options, |open| &mut open.options);
                    apply_named_option(
                        name,
                        &directive.value,
                        range,
                        destination,
                        &mut diagnostics,
                    );
                }
            }
        } else if let Some(open) = current.as_mut() {
            open.push_line(stripped);
        } else if !saw_filename {
            if !leading_body.is_empty() {
                leading_body.push('\n');
            }
            leading_body.push_str(stripped);
        }

        byte += raw_line.len();
        utf16 += utf16_len(raw_line);
        if has_nl {
            byte += 1;
            utf16 += 1;
            rest = &rest[raw_line.len() + 1..];
        } else {
            rest = "";
        }
        let _ = (line_start_byte, line_start_utf16);
    }

    if let Some(open) = current.take() {
        files.push(open.into_file());
    }

    if files.is_empty() && saw_filename {
        files.push(VirtualFile {
            script_kind: ScriptKind::TypeScript,
            path: "input.ts".to_owned(),
            text: String::new(),
            origin_start: Utf16Pos::ZERO,
            origin_byte: 0,
            options: DirectiveOptions::default(),
        });
    }

    if !saw_filename {
        let path = match validate_virtual_path(origin_name) {
            Ok(path) => path,
            Err(code) => {
                diagnostics.push(
                    error(
                        code,
                        empty_range(Utf16Pos::ZERO),
                        "origin virtual path is not confined",
                    )
                    .with_note(origin_name.to_owned()),
                );
                "input.ts".to_owned()
            }
        };
        files.push(VirtualFile {
            script_kind: script_kind_for_path(&path),
            path,
            text: leading_body,
            origin_start: Utf16Pos::ZERO,
            options: DirectiveOptions::default(),
            origin_byte: 0,
        });
    }

    Recovered::new(ParsedDirectives { files, options }, diagnostics)
}

struct OpenFile {
    path: String,
    body: String,
    origin_start: Utf16Pos,
    origin_byte: usize,
    options: DirectiveOptions,
}

impl OpenFile {
    fn push_line(&mut self, line: &str) {
        if !self.body.is_empty() {
            self.body.push('\n');
        }
        self.body.push_str(line);
    }

    fn into_file(self) -> VirtualFile {
        VirtualFile {
            script_kind: script_kind_for_path(&self.path),
            path: self.path,
            text: self.body,
            origin_start: self.origin_start,
            origin_byte: self.origin_byte,
            options: self.options,
        }
    }
}

struct Directive {
    name: String,
    value: String,
}

fn parse_directive_line(line: &str) -> Option<Directive> {
    let rest = line.trim_start();
    if !rest.starts_with("//") {
        return None;
    }
    let rest = rest[2..].trim_start();
    if !rest.starts_with('@') {
        return None;
    }
    let rest = &rest[1..];
    let colon = rest.find(':')?;
    let name = rest[..colon].trim();
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return None;
    }
    Some(Directive {
        name: name.to_ascii_lowercase(),
        value: rest[colon + 1..].trim().to_owned(),
    })
}

fn accept_filename(
    value: &str,
    range: TextRange,
    seen_paths: &mut BTreeSet<String>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<String> {
    match validate_virtual_path(value) {
        Ok(path) => {
            if !seen_paths.insert(path.clone()) {
                diagnostics.push(
                    error(
                        DUPLICATE_VIRTUAL_PATH,
                        range,
                        "duplicate @filename virtual path",
                    )
                    .with_note(path),
                );
                None
            } else {
                Some(path)
            }
        }
        Err(EMPTY_VIRTUAL_PATH) => {
            diagnostics.push(error(
                EMPTY_VIRTUAL_PATH,
                range,
                "@filename directive is missing a path",
            ));
            None
        }
        Err(_) => {
            diagnostics.push(
                error(
                    PATH_ESCAPES_PROJECT,
                    range,
                    "virtual path escapes the project root",
                )
                .with_note(value.to_owned()),
            );
            None
        }
    }
}

fn apply_link(value: &str, range: TextRange, diagnostics: &mut Vec<Diagnostic>) {
    if value.is_empty() {
        diagnostics.push(error(
            EMPTY_VIRTUAL_PATH,
            range,
            "@link directive is missing a path",
        ));
        return;
    }
    if validate_virtual_path(value).is_err() {
        diagnostics.push(
            error(
                PATH_ESCAPES_PROJECT,
                range,
                "virtual path escapes the project root",
            )
            .with_note(value.to_owned()),
        );
    }
}

fn apply_flags(
    value: &str,
    range: TextRange,
    options: &mut DirectiveOptions,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if value.is_empty() {
        diagnostics.push(error(
            MALFORMED_FLAGS,
            range,
            "@flags directive is missing a flag list",
        ));
        return;
    }
    let tokens: Vec<&str> = value.split_whitespace().collect();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        let rest = token.strip_prefix("--").or_else(|| token.strip_prefix('/'));
        let Some(rest) = rest else {
            diagnostics.push(
                error(MALFORMED_FLAGS, range, "@flags entries must be CLI flags")
                    .with_note(token.to_owned()),
            );
            index += 1;
            continue;
        };
        if rest.is_empty() {
            diagnostics.push(error(
                MALFORMED_FLAGS,
                range,
                "@flags entries must be CLI flags",
            ));
            index += 1;
            continue;
        }
        let (raw_name, inline) = match rest.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (rest, None),
        };
        let (name, default_value) = normalize_flag_name(raw_name);
        let implicit_value = if is_boolean_option(&name) {
            default_value
        } else {
            ""
        };
        if name.is_empty() {
            diagnostics.push(
                error(MALFORMED_FLAGS, range, "@flags entries must be CLI flags")
                    .with_note(token.to_owned()),
            );
            index += 1;
            continue;
        }
        let assigned = if let Some(inline) = inline {
            inline.to_owned()
        } else if let Some(next) = tokens.get(index + 1).copied() {
            if next.starts_with("--")
                || next.starts_with('/')
                || is_boolean_option(&name)
                    && !next.eq_ignore_ascii_case("true")
                    && !next.eq_ignore_ascii_case("false")
            {
                implicit_value.to_owned()
            } else {
                index += 1;
                next.to_owned()
            }
        } else {
            implicit_value.to_owned()
        };
        apply_named_option(&name, &assigned, range, options, diagnostics);
        index += 1;
    }
}

fn normalize_flag_name(raw: &str) -> (String, &'static str) {
    let trimmed = raw.trim();
    let canonical = canonical_option_name(trimmed);
    if is_known_option(&canonical) {
        return (canonical, "true");
    }
    if let Some(rest) = trimmed
        .strip_prefix("no-")
        .or_else(|| trimmed.strip_prefix("no"))
        && !rest.is_empty()
        && rest.as_bytes()[0].is_ascii_alphabetic()
    {
        let candidate = canonical_option_name(rest);
        if is_known_option(&candidate) && is_boolean_option(&candidate) {
            return (candidate, "false");
        }
    }
    (canonical, "true")
}

fn canonical_option_name(name: &str) -> String {
    name.bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .map(|byte| char::from(byte.to_ascii_lowercase()))
        .collect()
}

fn apply_named_option(
    name: &str,
    value: &str,
    range: TextRange,
    options: &mut DirectiveOptions,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let canonical = canonical_option_name(name);
    if canonical.is_empty() {
        diagnostics.push(error(UNKNOWN_DIRECTIVE, range, "unknown directive"));
        return;
    }
    if !is_known_option(&canonical) {
        diagnostics.push(
            error(UNKNOWN_DIRECTIVE, range, "unknown directive").with_note(format!("@{canonical}")),
        );
        return;
    }
    match validate_option_value(&canonical, value) {
        Ok(canonical_value) => options.insert(canonical, canonical_value),
        Err(message) => diagnostics
            .push(error(INVALID_OPTION_VALUE, range, "invalid directive value").with_note(message)),
    }
}

fn is_per_file_option(name: &str) -> bool {
    matches!(
        canonical_option_name(name).as_str(),
        "jsx" | "jsxfactory" | "jsxfragmentfactory" | "jsximportsource" | "tsximportsource"
    )
}

fn is_known_option(name: &str) -> bool {
    matches!(
        name,
        "allowarbitraryextensions"
            | "allowimportingtsextensions"
            | "allowjs"
            | "allowsyntheticdefaultimports"
            | "allowumdglobalaccess"
            | "allowunreachablecode"
            | "allowunusedlabels"
            | "alwaysstrict"
            | "assumechangesonlyaffectdirectdependencies"
            | "baseurl"
            | "charset"
            | "checkjs"
            | "composite"
            | "customconditions"
            | "declaration"
            | "declarationdir"
            | "declarationmap"
            | "diagnostics"
            | "disablereferencedprojectload"
            | "disablesizelimit"
            | "disablesolutionsearching"
            | "disablesourceofprojectreferenceredirect"
            | "downleveliteration"
            | "emitbom"
            | "emitdeclarationonly"
            | "emitdecoratormetadata"
            | "erasablesyntaxonly"
            | "esmoduleinterop"
            | "exactoptionalpropertytypes"
            | "experimentaldecorators"
            | "extendeddiagnostics"
            | "forceconsistentcasinginfilenames"
            | "generatecpuprofile"
            | "ignoredeprecations"
            | "importhelpers"
            | "importsnotusedasvalues"
            | "incremental"
            | "inlinesourcemap"
            | "inlinesources"
            | "isolateddeclarations"
            | "isolatedmodules"
            | "jsx"
            | "jsxfactory"
            | "jsxfragmentfactory"
            | "jsximportsource"
            | "tsximportsource"
            | "lib"
            | "libreplacement"
            | "listemittedfiles"
            | "listfiles"
            | "locale"
            | "maproot"
            | "maxnodemodulejsdepth"
            | "module"
            | "moduledetection"
            | "moduleresolution"
            | "modulesuffixes"
            | "newline"
            | "nocheck"
            | "noemit"
            | "noemithelpers"
            | "noemitonerror"
            | "noerrortruncation"
            | "nofallthroughcasesinswitch"
            | "noimplicitany"
            | "noimplicitoverride"
            | "noimplicitreturns"
            | "noimplicitthis"
            | "nolib"
            | "nopropertyaccessfromindexsignature"
            | "noresolve"
            | "nostrictgenericchecks"
            | "nouncheckedindexedaccess"
            | "nouncheckedsideeffectimports"
            | "nounusedlocals"
            | "nounusedparameters"
            | "outdir"
            | "outfile"
            | "paths"
            | "plugins"
            | "preserveconstenums"
            | "preservesymlinks"
            | "preservevalueimports"
            | "pretty"
            | "reactnamespace"
            | "removecomments"
            | "resolvejsonmodule"
            | "rewriterelativeimportextensions"
            | "rootdir"
            | "rootdirs"
            | "skipdefaultlibcheck"
            | "skiplibcheck"
            | "sourcemap"
            | "sourceroot"
            | "strict"
            | "strictbindcallapply"
            | "strictbuiltiniteratorreturn"
            | "strictfunctiontypes"
            | "strictnullchecks"
            | "strictpropertyinitialization"
            | "stripinternal"
            | "suppressexcesspropertyerrors"
            | "suppressimplicitanyindexerrors"
            | "target"
            | "tracesresolution"
            | "tsbuildinfofile"
            | "typeroots"
            | "types"
            | "usecasesensitivefilenames"
            | "usedefineforclassfields"
            | "useunknownincatchvariables"
            | "verbatimmodulesyntax"
    )
}

fn is_boolean_option(name: &str) -> bool {
    !matches!(
        name,
        "baseurl"
            | "charset"
            | "customconditions"
            | "declarationdir"
            | "generatecpuprofile"
            | "ignoredeprecations"
            | "importsnotusedasvalues"
            | "jsx"
            | "jsxfactory"
            | "jsxfragmentfactory"
            | "jsximportsource"
            | "tsximportsource"
            | "lib"
            | "locale"
            | "maproot"
            | "maxnodemodulejsdepth"
            | "module"
            | "moduledetection"
            | "moduleresolution"
            | "modulesuffixes"
            | "newline"
            | "outdir"
            | "outfile"
            | "paths"
            | "plugins"
            | "reactnamespace"
            | "rootdir"
            | "rootdirs"
            | "sourceroot"
            | "target"
            | "tsbuildinfofile"
            | "typeroots"
            | "types"
    )
}

fn validate_option_value(name: &str, value: &str) -> Result<String, String> {
    match name {
        "target" => {
            if is_target(value) {
                Ok(value.to_ascii_lowercase())
            } else {
                Err(format!("unknown @target value `{value}`"))
            }
        }
        "module" => {
            if is_module(value) {
                Ok(value.to_ascii_lowercase())
            } else {
                Err(format!("unknown @module value `{value}`"))
            }
        }
        "jsx" => {
            if is_jsx(value) {
                Ok(normalize_jsx(value))
            } else {
                Err(format!("unknown @jsx value `{value}`"))
            }
        }
        "moduleresolution" => {
            if is_module_resolution(value) {
                Ok(normalize_module_resolution(value))
            } else {
                Err(format!("unknown @moduleResolution value `{value}`"))
            }
        }
        "lib" => {
            if value.is_empty() {
                Err("@lib directive is missing a value".to_owned())
            } else {
                Ok(value.to_owned())
            }
        }
        _ if is_boolean_option(name) => {
            if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false") {
                Ok(value.to_ascii_lowercase())
            } else {
                Err(format!(
                    "directive @{name} expects true or false, found {value}"
                ))
            }
        }
        _ => {
            if value.is_empty() {
                Err(format!("directive `@{name}` is missing a value"))
            } else {
                Ok(value.to_owned())
            }
        }
    }
}

fn is_target(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "es3"
            | "es5"
            | "es6"
            | "es2015"
            | "es2016"
            | "es2017"
            | "es2018"
            | "es2019"
            | "es2020"
            | "es2021"
            | "es2022"
            | "es2023"
            | "es2024"
            | "es2025"
            | "esnext"
    )
}

fn is_module(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "none"
            | "commonjs"
            | "amd"
            | "umd"
            | "system"
            | "es6"
            | "es2015"
            | "es2020"
            | "es2022"
            | "esnext"
            | "node16"
            | "node18"
            | "node20"
            | "nodenext"
            | "preserve"
    )
}

fn is_jsx(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "preserve"
            | "react"
            | "react-native"
            | "reactnative"
            | "react-jsx"
            | "reactjsx"
            | "react-jsxdev"
            | "reactjsxdev"
    )
}

fn normalize_jsx(value: &str) -> String {
    match value.to_ascii_lowercase().as_str() {
        "reactnative" => "react-native".to_owned(),
        "reactjsx" => "react-jsx".to_owned(),
        "reactjsxdev" => "react-jsxdev".to_owned(),
        other => other.to_owned(),
    }
}

fn is_module_resolution(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "classic" | "node" | "node10" | "node16" | "nodenext" | "bundler"
    )
}

fn normalize_module_resolution(value: &str) -> String {
    match value.to_ascii_lowercase().as_str() {
        "node" => "node10".to_owned(),
        other => other.to_owned(),
    }
}

fn parse_bool_value(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn validate_virtual_path(raw: &str) -> Result<String, DiagnosticCode> {
    if raw.is_empty() {
        return Err(EMPTY_VIRTUAL_PATH);
    }
    if raw.contains('\0') {
        return Err(PATH_ESCAPES_PROJECT);
    }
    let bytes = raw.as_bytes();
    if raw.starts_with("\\")
        || bytes
            .get(1)
            .is_some_and(|separator| *separator == b':' && bytes[0].is_ascii_alphabetic())
    {
        return Err(PATH_ESCAPES_PROJECT);
    }
    let normalized = raw.replace('\\', "/");
    let candidate = Path::new(&normalized);
    if candidate.is_absolute() {
        return Err(PATH_ESCAPES_PROJECT);
    }
    let mut parts = Vec::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => {
                let Some(part) = part.to_str() else {
                    return Err(PATH_ESCAPES_PROJECT);
                };
                parts.push(part);
            }
            _ => return Err(PATH_ESCAPES_PROJECT),
        }
    }
    if parts.is_empty() {
        return Err(PATH_ESCAPES_PROJECT);
    }
    Ok(parts.join("/"))
}

fn script_kind_for_path(path: &str) -> ScriptKind {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".tsx") {
        ScriptKind::TypeScriptReact
    } else if lower.ends_with(".jsx") {
        ScriptKind::JavaScriptReact
    } else if lower.ends_with(".js") || lower.ends_with(".mjs") || lower.ends_with(".cjs") {
        ScriptKind::JavaScript
    } else if lower.ends_with(".json") {
        ScriptKind::Json
    } else {
        ScriptKind::TypeScript
    }
}

fn is_ws_only(text: &str) -> bool {
    text.chars().all(char::is_whitespace)
}

fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

fn line_range(start: usize, raw_line: &str) -> TextRange {
    TextRange::new(
        Utf16Pos::new(start),
        Utf16Pos::new(start + utf16_len(raw_line)),
    )
    .unwrap_or_else(|_| empty_range(Utf16Pos::new(start)))
}

fn empty_range(at: Utf16Pos) -> TextRange {
    TextRange::new(at, at).expect("an empty range is ordered")
}

fn error(code: DiagnosticCode, range: TextRange, message: &'static str) -> Diagnostic {
    Diagnostic::error(code, SOURCE, range, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::DiagnosticSeverity;

    fn parse(source: &str) -> Recovered<ParsedDirectives> {
        parse_tsc_directives(source, "case.ts")
    }

    fn errors(recovered: &Recovered<ParsedDirectives>) -> Vec<&Diagnostic> {
        recovered
            .diagnostics()
            .iter()
            .filter(|d| d.severity() == DiagnosticSeverity::Error)
            .collect()
    }

    #[test]
    fn single_file_without_filename_keeps_origin_path() {
        let recovered = parse("const x = 1;\n");
        assert!(errors(&recovered).is_empty());
        let files = recovered.product().files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path(), "case.ts");
        assert_eq!(files[0].text(), "const x = 1;");
        assert_eq!(files[0].origin_start(), Utf16Pos::ZERO);
        assert_eq!(files[0].origin_byte(), 0);
        assert_eq!(files[0].script_kind(), ScriptKind::TypeScript);
    }

    #[test]
    fn strips_option_directives_from_a_single_file_body() {
        let recovered = parse("// @strict: true\nconst x = 1;\n");
        assert!(errors(&recovered).is_empty());
        let product = recovered.product();
        assert_eq!(product.options().strict(), Some(true));
        assert_eq!(product.files()[0].text(), "const x = 1;");
    }

    #[test]
    fn splits_filename_virtual_files_with_original_offsets() {
        let source =
            "// @filename: a.ts\nexport const a = 1;\n// @filename: b.tsx\nexport const b = 2;\n";
        let recovered = parse(source);
        assert!(errors(&recovered).is_empty(), "{:?}", errors(&recovered));
        let files = recovered.product().files();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path(), "a.ts");
        assert_eq!(files[0].text(), "export const a = 1;");
        assert_eq!(files[0].script_kind(), ScriptKind::TypeScript);
        assert_eq!(files[1].path(), "b.tsx");
        assert_eq!(files[1].text(), "export const b = 2;");
        assert_eq!(files[1].script_kind(), ScriptKind::TypeScriptReact);
        let first_line = "// @filename: a.ts\n";
        assert_eq!(files[0].origin_start().get(), utf16_len(first_line));
        assert_eq!(files[0].origin_byte(), first_line.len());
        assert_eq!(
            &source[files[0].origin_byte()..files[0].origin_byte() + files[0].text().len()],
            files[0].text()
        );
    }

    #[test]
    fn file_jsx_directives_override_globals_deterministically() {
        let recovered = parse(
            "// @jsx: react\n\
             // @jsxImportSource: global-jsx\n\
             // @tsxImportSource: global-tsx\n\
             // @filename: local.tsx\n\
             // @jsx: preserve\n\
             // @jsxImportSource: local-generic\n\
             export const local = <div />;\n\
             // @filename: inherited.tsx\n\
             export const inherited = <div />;\n\
             // @filename: javascript.jsx\n\
             export const javascript = <div />;\n",
        );
        assert!(errors(&recovered).is_empty(), "{:?}", errors(&recovered));
        let product = recovered.product();
        let [local, inherited, javascript] = product.files() else {
            panic!("expected three virtual files")
        };

        assert_eq!(
            product.jsx_for(local, Some(JsxEmit::ReactJsxDev)),
            Some(JsxEmit::Preserve)
        );
        assert_eq!(
            product.jsx_import_source_for(local, Some("project")),
            Some("local-generic")
        );
        assert_eq!(
            product.jsx_for(inherited, Some(JsxEmit::ReactJsxDev)),
            Some(JsxEmit::React)
        );
        assert_eq!(
            product.jsx_import_source_for(inherited, Some("project")),
            Some("global-tsx")
        );
        assert_eq!(
            product.jsx_import_source_for(javascript, Some("project")),
            Some("global-jsx")
        );
    }

    #[test]
    fn flags_directive_feeds_cli_shaped_options() {
        let recovered =
            parse("// @flags: --strict --target esnext --module commonjs\nconst x = 1;\n");
        assert!(errors(&recovered).is_empty(), "{:?}", errors(&recovered));
        let options = recovered.product().options();
        assert_eq!(options.strict(), Some(true));
        assert_eq!(options.target(), Some("esnext"));
        assert_eq!(options.module(), Some("commonjs"));
        let flags = options.to_cli_flags();
        assert!(flags.iter().any(|flag| flag == "--strict"));
        assert!(flags.iter().any(|flag| flag == "--target"));
        assert!(flags.iter().any(|flag| flag == "esnext"));
    }

    #[test]
    fn empty_filename_is_a_typed_error() {
        let recovered = parse("// @filename:\nconst x = 1;\n");
        assert!(
            errors(&recovered)
                .iter()
                .any(|d| d.code() == EMPTY_VIRTUAL_PATH)
        );
    }

    #[test]
    fn duplicate_filename_is_a_typed_error() {
        let recovered = parse("// @filename: a.ts\nx\n// @filename: a.ts\ny\n");
        assert!(
            errors(&recovered)
                .iter()
                .any(|d| d.code() == DUPLICATE_VIRTUAL_PATH)
        );
    }

    #[test]
    fn escaping_filename_is_a_typed_error() {
        let recovered = parse("// @filename: ../secret.ts\nconst x = 1;\n");
        assert!(
            errors(&recovered)
                .iter()
                .any(|d| d.code() == PATH_ESCAPES_PROJECT)
        );
        let recovered = parse("// @filename: /abs.ts\nconst x = 1;\n");
        assert!(
            errors(&recovered)
                .iter()
                .any(|d| d.code() == PATH_ESCAPES_PROJECT)
        );
    }

    #[test]
    fn content_before_filename_is_a_typed_error() {
        let recovered = parse("const x = 1;\n// @filename: a.ts\nexport {};\n");
        assert!(
            errors(&recovered)
                .iter()
                .any(|d| d.code() == CONTENT_BEFORE_FILENAME)
        );
    }

    #[test]
    fn unknown_directive_and_invalid_values_fail() {
        let recovered = parse("// @notarealoption: true\nconst x = 1;\n");
        assert!(
            errors(&recovered)
                .iter()
                .any(|d| d.code() == UNKNOWN_DIRECTIVE)
        );
        let recovered = parse("// @target: widgets\nconst x = 1;\n");
        assert!(
            errors(&recovered)
                .iter()
                .any(|d| d.code() == INVALID_OPTION_VALUE)
        );
        let recovered = parse("// @strict: maybe\nconst x = 1;\n");
        assert!(
            errors(&recovered)
                .iter()
                .any(|d| d.code() == INVALID_OPTION_VALUE)
        );
        let recovered = parse("// @flags: target esnext\nconst x = 1;\n");
        assert!(
            errors(&recovered)
                .iter()
                .any(|d| d.code() == MALFORMED_FLAGS)
        );
    }

    #[test]
    fn whitespace_only_lines_before_filename_are_accepted() {
        let recovered = parse("\n  \n// @filename: a.ts\nexport {};\n");
        assert!(
            errors(&recovered).is_empty(),
            "{:?}",
            errors(&recovered)
                .iter()
                .map(|d| d.code().as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(recovered.product().files()[0].path(), "a.ts");
    }

    #[test]
    fn nostrict_flag_is_false() {
        let recovered = parse("// @flags: --noStrict\nconst x = 1;\n");
        assert!(errors(&recovered).is_empty(), "{:?}", errors(&recovered));
        assert_eq!(recovered.product().options().strict(), Some(false));
    }

    #[test]
    fn nul_and_link_paths_are_typed_errors() {
        let recovered = parse("// @filename: a\0.ts\nconst x = 1;\n");
        assert!(
            errors(&recovered)
                .iter()
                .any(|d| d.code() == PATH_ESCAPES_PROJECT)
        );
        let recovered = parse("// @link: ../secret.ts\nconst x = 1;\n");
        assert!(
            errors(&recovered)
                .iter()
                .any(|d| d.code() == PATH_ESCAPES_PROJECT)
        );
    }

    #[test]
    fn duplicate_filename_keeps_the_first_file_only() {
        let recovered = parse("// @filename: a.ts\nx\n// @filename: a.ts\ny\n");
        assert_eq!(recovered.product().files().len(), 1);
        assert_eq!(recovered.product().files()[0].text(), "x");
    }

    #[test]
    fn mixed_case_filename_directive_splits() {
        let recovered = parse("// @Filename: A.ts\nexport {};\n");
        assert!(errors(&recovered).is_empty(), "{:?}", errors(&recovered));
        assert_eq!(recovered.product().files()[0].path(), "A.ts");
    }

    #[test]
    fn empty_filename_still_yields_a_recovered_file() {
        let recovered = parse("// @filename:\nconst x = 1;\n");
        assert!(!recovered.product().files().is_empty());
    }

    #[test]
    fn no_prefixed_declared_flags_are_not_boolean_negations() {
        let recovered = parse("// @flags: --noEmit --noLib --noStrict\nconst x = 1;\n");
        assert!(errors(&recovered).is_empty(), "{:?}", errors(&recovered));
        let options = recovered.product().options();
        assert_eq!(options.get("noEmit"), Some("true"));
        assert_eq!(options.get("noLib"), Some("true"));
        assert_eq!(options.strict(), Some(false));
    }

    #[test]
    fn bom_and_crlf_before_filename_preserve_origin_offsets() {
        let source = "\u{feff}// @filename: a.ts\r\nconst x = 1;\r\n";
        let recovered = parse(source);
        assert!(errors(&recovered).is_empty(), "{:?}", errors(&recovered));
        let file = &recovered.product().files()[0];
        assert_eq!(file.path(), "a.ts");
        assert_eq!(file.text(), "const x = 1;");
        assert_eq!(
            file.origin_start().get(),
            utf16_len("\u{feff}// @filename: a.ts\r\n")
        );
        assert_eq!(file.origin_byte(), "\u{feff}// @filename: a.ts\r\n".len());
    }

    #[test]
    fn windows_paths_are_normalized_and_confined() {
        let recovered = parse("// @filename: src\\a.ts\nexport {};\n");
        assert!(errors(&recovered).is_empty(), "{:?}", errors(&recovered));
        assert_eq!(recovered.product().files()[0].path(), "src/a.ts");

        for path in [r"C:\secret.ts", r"\\server\share\secret.ts"] {
            let recovered = parse(&format!("// @filename: {path}\nexport {{}};\n"));
            assert!(
                errors(&recovered)
                    .iter()
                    .any(|diagnostic| diagnostic.code() == PATH_ESCAPES_PROJECT),
                "{path} was not rejected"
            );
        }
    }
    #[test]
    fn current_compiler_options_accept_boolean_and_value_forms() {
        let recovered = parse(
            "// @verbatimModuleSyntax: true\n\
             // @moduleDetection: force\n\
             // @flags: --rewriteRelativeImportExtensions --outDir build --types node,bun --strict FALSE\n\
             export {};\n",
        );
        assert!(errors(&recovered).is_empty(), "{:?}", errors(&recovered));
        let options = recovered.product().options();
        assert_eq!(options.get("verbatimModuleSyntax"), Some("true"));
        assert_eq!(options.get("moduleDetection"), Some("force"));
        assert_eq!(options.get("rewriteRelativeImportExtensions"), Some("true"));
        assert_eq!(options.get("outDir"), Some("build"));
        assert_eq!(options.get("types"), Some("node,bun"));
        assert_eq!(options.strict(), Some(false));
    }

    #[test]
    fn value_flags_require_values_without_swallowing_following_flags() {
        let recovered = parse("// @flags: --outDir --strict\nexport {};\n");
        assert!(
            errors(&recovered)
                .iter()
                .any(|diagnostic| diagnostic.code() == INVALID_OPTION_VALUE)
        );
        assert_eq!(recovered.product().options().strict(), Some(true));
        assert_eq!(recovered.product().options().get("outDir"), None);
    }
}
