pub mod semantic;

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    checker::{ProgramSemanticModel, SemanticModel},
    diagnostic::Diagnostic,
    lint::{CompilerLintOptions, LintProfile, LintTable, SourceDialect, rule_by_code},
    source::{ScriptKind, SourceId, TextRange, Utf16Pos},
    syntax::{
        ClassMember, ExportDeclaration, FunctionBody, FunctionLike, InterfaceDeclaration,
        SourceFile, Statement, Stmt, TokenKind, TypeMember, TypeNode,
    },
};

#[derive(Clone, Copy)]
struct SyntaxToken<'a> {
    kind: TokenKind,
    text: &'a str,
    range: TextRange,
}

impl SyntaxToken<'_> {
    fn is(self, text: &str) -> bool {
        self.text == text
    }

    fn identifier(self) -> bool {
        matches!(
            self.kind,
            TokenKind::Identifier | TokenKind::PrivateIdentifier
        )
    }
}

/// Runs every syntax-only lint enabled by `levels`.
///
/// Rule identity and severity come exclusively from [`crate::lint::RULES`].
#[must_use]
pub fn analyze(source: &SourceFile, levels: &LintTable) -> Vec<Diagnostic> {
    let tokens = source
        .tokens()
        .iter()
        .filter(|token| {
            !matches!(
                token.kind(),
                TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment
            )
        })
        .filter_map(|token| {
            Some(SyntaxToken {
                kind: token.kind(),
                text: source.token_text(token)?,
                range: token.range(),
            })
        })
        .collect::<Vec<_>>();
    let comments = source
        .tokens()
        .iter()
        .filter(|token| {
            matches!(
                token.kind(),
                TokenKind::LineComment | TokenKind::BlockComment
            )
        })
        .filter_map(|token| {
            Some(SyntaxToken {
                kind: token.kind(),
                text: source.token_text(token)?,
                range: token.range(),
            })
        })
        .collect::<Vec<_>>();
    let mut findings = Vec::<(&'static str, TextRange, &'static str)>::new();

    find_escape_hatches(&tokens, &comments, &mut findings);
    find_non_erasable_and_legacy(source.script_kind(), &tokens, &mut findings);
    find_modules(&tokens, &mut findings);
    find_class_fields(&tokens, &mut findings);
    find_enums(&tokens, &mut findings);
    find_declaration_merges(&tokens, &mut findings);
    find_hygiene(&tokens, &mut findings);
    find_syntactic_footguns(&tokens, &mut findings);
    find_catalog_completion(source, &tokens, &comments, &mut findings);

    let dialect = match source.script_kind() {
        ScriptKind::JavaScript | ScriptKind::JavaScriptReact => SourceDialect::JavaScript,
        ScriptKind::TypeScript | ScriptKind::TypeScriptReact | ScriptKind::Json => {
            SourceDialect::TypeScript
        }
    };
    let mut diagnostics = findings
        .into_iter()
        .filter_map(|(code, range, message)| {
            let rule = rule_by_code(code).expect("syntax rule code must be registered");
            Diagnostic::lint(
                levels.level_for_source(rule.id(), dialect),
                rule.id(),
                source.source_id(),
                range,
                message,
            )
        })
        .collect::<Vec<_>>();
    diagnostics.sort();
    diagnostics
}

/// Runs every checker-dependent rule over the frozen semantic model.
#[must_use]
pub fn analyze_semantic(
    source: &SourceFile,
    model: &SemanticModel,
    program: Option<&ProgramSemanticModel>,
    levels: &LintTable,
) -> Vec<Diagnostic> {
    semantic::analyze(source, model, program, levels)
}

/// Runs syntax rules with their settled default levels.
#[must_use]
pub fn analyze_default(source: &SourceFile) -> Vec<Diagnostic> {
    analyze(source, &LintTable::new(LintProfile::Default))
}

/// Runs compiler-option rules at the configuration boundary.
#[must_use]
pub fn analyze_compiler_options(
    options: CompilerLintOptions,
    levels: &LintTable,
    source_id: SourceId,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for (enabled, code, message) in [
        (
            options.preserve_const_enums,
            "BAMTS-W082",
            "preserveConstEnums retains runtime enum objects while inlining uses",
        ),
        (
            options.emit_decorator_metadata,
            "BAMTS-W083",
            "emitDecoratorMetadata couples runtime reflection to compiler types",
        ),
        (
            !options.use_define_for_class_fields,
            "BAMTS-W084",
            "useDefineForClassFields=false selects legacy setter-invoking semantics",
        ),
    ] {
        if !enabled {
            continue;
        }
        let rule = rule_by_code(code).expect("configuration rule code must be registered");
        if let Some(diagnostic) = Diagnostic::lint(
            levels.level(rule.id()),
            rule.id(),
            source_id,
            TextRange::new(Utf16Pos::ZERO, Utf16Pos::ZERO).expect("zero range is valid"),
            message,
        ) {
            diagnostics.push(diagnostic);
        }
    }
    diagnostics.sort();
    diagnostics
}

fn push(
    findings: &mut Vec<(&'static str, TextRange, &'static str)>,
    code: &'static str,
    token: SyntaxToken<'_>,
    message: &'static str,
) {
    findings.push((code, token.range, message));
}

fn find_escape_hatches(
    tokens: &[SyntaxToken<'_>],
    comments: &[SyntaxToken<'_>],
    findings: &mut Vec<(&'static str, TextRange, &'static str)>,
) {
    for (index, token) in tokens.iter().copied().enumerate() {
        if token.kind == TokenKind::KwAny {
            push(
                findings,
                "BAMTS-W017",
                token,
                "explicit any bypasses static checking",
            );
        }
        if token.kind == TokenKind::KwAs
            && tokens
                .get(index + 1)
                .is_some_and(|next| next.is("any") || next.is("unknown"))
            && tokens
                .get(index + 2)
                .is_some_and(|next| next.kind == TokenKind::KwAs)
        {
            push(
                findings,
                "BAMTS-W020",
                token,
                "double assertion bypasses type compatibility",
            );
        }
        if token.kind == TokenKind::Bang
            && index > 0
            && tokens[index - 1].identifier()
            && !tokens
                .get(index + 1)
                .is_some_and(|next| next.kind == TokenKind::Colon)
        {
            push(
                findings,
                "BAMTS-W021",
                token,
                "non-null assertion bypasses nullability proof",
            );
        }
        if token.kind == TokenKind::Bang
            && index > 0
            && tokens[index - 1].identifier()
            && tokens
                .get(index + 1)
                .is_some_and(|next| next.kind == TokenKind::Colon)
        {
            push(
                findings,
                "BAMTS-W022",
                token,
                "definite-assignment assertion bypasses initialization proof",
            );
        }
    }
    for comment in comments {
        if ["@ts-ignore", "@ts-expect-error", "@ts-nocheck"]
            .iter()
            .any(|directive| comment.text.contains(directive))
        {
            push(
                findings,
                "BAMTS-W023",
                *comment,
                "diagnostic suppression directive hides compiler findings",
            );
        }
    }
}

fn find_non_erasable_and_legacy(
    script_kind: ScriptKind,
    tokens: &[SyntaxToken<'_>],
    findings: &mut Vec<(&'static str, TextRange, &'static str)>,
) {
    for (index, token) in tokens.iter().copied().enumerate() {
        if token.kind == TokenKind::KwNamespace
            && let Some((start, end)) =
                delimited_after(tokens, index, TokenKind::LBrace, TokenKind::RBrace)
            && tokens[start + 1..end].iter().any(|inner| {
                matches!(
                    inner.kind,
                    TokenKind::KwConst
                        | TokenKind::KwLet
                        | TokenKind::KwVar
                        | TokenKind::KwFunction
                        | TokenKind::KwClass
                        | TokenKind::KwEnum
                )
            })
        {
            push(
                findings,
                "BAMTS-W024",
                token,
                "runtime namespace requires non-erasable emit",
            );
        }
        if token.kind == TokenKind::KwConstructor
            && let Some((start, end)) =
                delimited_after(tokens, index, TokenKind::LParen, TokenKind::RParen)
            && tokens[start + 1..end].iter().any(|parameter| {
                matches!(
                    parameter.kind,
                    TokenKind::KwPublic
                        | TokenKind::KwPrivate
                        | TokenKind::KwProtected
                        | TokenKind::KwReadonly
                )
            })
        {
            push(
                findings,
                "BAMTS-W025",
                token,
                "parameter property requires runtime field synthesis",
            );
        }
        if token.kind == TokenKind::At {
            push(
                findings,
                "BAMTS-W026",
                token,
                "decorator syntax depends on legacy transform semantics",
            );
        }
        if token.kind == TokenKind::LessThan
            && assertion_can_start_at(tokens, index)
            && tokens.get(index + 1).is_some_and(|next| {
                next.identifier()
                    || matches!(
                        next.kind,
                        TokenKind::KwAny
                            | TokenKind::KwBigint
                            | TokenKind::KwBoolean
                            | TokenKind::KwNever
                            | TokenKind::KwNumber
                            | TokenKind::KwObject
                            | TokenKind::KwString
                            | TokenKind::KwSymbol
                            | TokenKind::KwUnknown
                    )
            })
            && tokens
                .get(index + 2)
                .is_some_and(|next| next.kind == TokenKind::GreaterThan)
            && tokens.get(index + 3).is_some_and(|next| {
                next.identifier()
                    || matches!(
                        next.kind,
                        TokenKind::NumericLiteral | TokenKind::StringLiteral | TokenKind::LParen
                    )
            })
        {
            push(
                findings,
                "BAMTS-W027",
                token,
                "angle-bracket assertion is ambiguous with JSX",
            );
        }
    }
    if matches!(
        script_kind,
        ScriptKind::TypeScriptReact | ScriptKind::JavaScriptReact
    ) {
        for (index, token) in tokens.iter().copied().enumerate() {
            if token.kind == TokenKind::LessThan && looks_like_jsx(tokens, index) {
                push(
                    findings,
                    "BAMTS-W029",
                    token,
                    "JSX syntax requires a runtime transform",
                );
                break;
            }
        }
    }
}

fn find_modules(
    tokens: &[SyntaxToken<'_>],
    findings: &mut Vec<(&'static str, TextRange, &'static str)>,
) {
    let esm = tokens.iter().enumerate().any(|(index, token)| {
        (token.kind == TokenKind::KwExport
            && !tokens
                .get(index + 1)
                .is_some_and(|next| next.kind == TokenKind::Eq))
            || (token.kind == TokenKind::KwImport
                && !tokens
                    .get(index + 1)
                    .is_some_and(|next| next.kind == TokenKind::LParen))
    });
    let common_js = tokens.iter().enumerate().find_map(|(index, token)| {
        ((token.is("require")
            && tokens
                .get(index + 1)
                .is_some_and(|next| next.kind == TokenKind::LParen))
            || (token.is("module")
                && tokens
                    .get(index + 1)
                    .is_some_and(|next| next.kind == TokenKind::Dot)
                && tokens.get(index + 2).is_some_and(|next| next.is("exports")))
            || (token.is("exports")
                && tokens
                    .get(index + 1)
                    .is_some_and(|next| next.kind == TokenKind::Dot)))
        .then_some(*token)
    });

    for (index, token) in tokens.iter().copied().enumerate() {
        if (token.kind == TokenKind::KwImport
            && tokens[index + 1..tokens.len().min(index + 8)]
                .iter()
                .any(|next| next.kind == TokenKind::Eq))
            || (token.kind == TokenKind::KwExport
                && tokens
                    .get(index + 1)
                    .is_some_and(|next| next.kind == TokenKind::Eq))
        {
            push(
                findings,
                "BAMTS-W030",
                token,
                "import/export equals requires module rewriting",
            );
        }
        if token.kind == TokenKind::StringLiteral
            && is_module_source(tokens, index)
            && extensionless_relative(token.text)
        {
            push(
                findings,
                "BAMTS-W036",
                token,
                "relative ESM import omits its runtime file extension",
            );
        }
    }
    if esm {
        if let Some(token) = common_js {
            push(
                findings,
                "BAMTS-W033",
                token,
                "CommonJS binding appears in an ES module",
            );
        }
    } else if let Some(token) = tokens.first().copied() {
        push(
            findings,
            "BAMTS-W034",
            token,
            "file has implicit script rather than module semantics",
        );
    }
}

fn find_class_fields(
    tokens: &[SyntaxToken<'_>],
    findings: &mut Vec<(&'static str, TextRange, &'static str)>,
) {
    for (class_index, class_token) in tokens.iter().copied().enumerate() {
        if class_token.kind != TokenKind::KwClass {
            continue;
        }
        let Some((start, end)) =
            delimited_after(tokens, class_index, TokenKind::LBrace, TokenKind::RBrace)
        else {
            continue;
        };
        let body = &tokens[start + 1..end];
        let mut nested = 0usize;
        for (index, token) in body.iter().copied().enumerate() {
            if matches!(
                token.kind,
                TokenKind::RBrace | TokenKind::RBracket | TokenKind::RParen
            ) {
                nested = nested.saturating_sub(1);
                continue;
            }
            if nested == 0 {
                if token.identifier()
                    && body
                        .get(index + 1)
                        .is_some_and(|next| next.kind == TokenKind::Colon)
                    && field_terminator_without_initializer(body, index + 2)
                {
                    push(
                        findings,
                        "BAMTS-W039",
                        token,
                        "uninitialized field has standard define-time runtime presence",
                    );
                }
                if matches!(token.kind, TokenKind::KwPrivate | TokenKind::KwProtected)
                    && body.get(index + 1).is_some_and(|next| next.identifier())
                    && body.get(index + 2).is_some_and(|next| {
                        matches!(
                            next.kind,
                            TokenKind::Colon | TokenKind::Eq | TokenKind::Semicolon
                        )
                    })
                {
                    push(
                        findings,
                        "BAMTS-W042",
                        token,
                        "TypeScript private field is erased rather than runtime-private",
                    );
                }
            }
            if matches!(
                token.kind,
                TokenKind::LBrace | TokenKind::LBracket | TokenKind::LParen
            ) {
                nested += 1;
            }
        }
    }
}

fn find_enums(
    tokens: &[SyntaxToken<'_>],
    findings: &mut Vec<(&'static str, TextRange, &'static str)>,
) {
    for (index, token) in tokens.iter().copied().enumerate() {
        if token.kind != TokenKind::KwEnum {
            continue;
        }
        let is_const = index > 0 && tokens[index - 1].kind == TokenKind::KwConst;
        push(
            findings,
            if is_const { "BAMTS-W044" } else { "BAMTS-W043" },
            token,
            if is_const {
                "const enum requires compile-time use inlining"
            } else {
                "non-const enum creates a runtime object"
            },
        );
        let Some((start, end)) =
            delimited_after(tokens, index, TokenKind::LBrace, TokenKind::RBrace)
        else {
            continue;
        };
        let mut kinds = BTreeSet::new();
        for member in split_top_level(&tokens[start + 1..end], TokenKind::Comma) {
            let Some(equal) = member.iter().position(|item| item.kind == TokenKind::Eq) else {
                continue;
            };
            let initializer = &member[equal + 1..];
            if let Some(first) = initializer.first().copied() {
                if first.kind == TokenKind::StringLiteral {
                    kinds.insert("string");
                } else if first.kind == TokenKind::NumericLiteral
                    || (first.kind == TokenKind::Minus
                        && initializer
                            .get(1)
                            .is_some_and(|next| next.kind == TokenKind::NumericLiteral))
                {
                    kinds.insert("number");
                } else if initializer
                    .iter()
                    .any(|item| item.kind == TokenKind::LParen)
                {
                    push(
                        findings,
                        "BAMTS-W047",
                        first,
                        "computed enum member is not a constant expression",
                    );
                }
            }
        }
        if kinds.len() > 1 {
            push(
                findings,
                "BAMTS-W046",
                token,
                "enum mixes string and numeric members",
            );
        }
    }
}

fn find_declaration_merges(
    tokens: &[SyntaxToken<'_>],
    findings: &mut Vec<(&'static str, TextRange, &'static str)>,
) {
    let mut interfaces = BTreeMap::<&str, SyntaxToken<'_>>::new();
    let mut values = BTreeMap::<&str, SyntaxToken<'_>>::new();
    let mut namespaces = BTreeMap::<&str, SyntaxToken<'_>>::new();
    for (index, token) in tokens.iter().copied().enumerate() {
        let Some(name) = tokens
            .get(index + 1)
            .copied()
            .filter(|next| next.identifier())
        else {
            continue;
        };
        match token.kind {
            TokenKind::KwInterface => {
                if interfaces.insert(name.text, name).is_some() {
                    push(
                        findings,
                        "BAMTS-W049",
                        name,
                        "same-scope interface declarations merge",
                    );
                }
            }
            TokenKind::KwClass | TokenKind::KwFunction | TokenKind::KwEnum => {
                values.insert(name.text, name);
            }
            TokenKind::KwNamespace => {
                namespaces.insert(name.text, name);
            }
            _ => {}
        }
    }
    for (name, token) in namespaces {
        if values.contains_key(name) {
            push(
                findings,
                "BAMTS-W050",
                token,
                "namespace merges with a runtime value declaration",
            );
        }
    }
    for (index, token) in tokens.iter().copied().enumerate() {
        if token.kind != TokenKind::KwDeclare {
            continue;
        }
        match tokens.get(index + 1).map(|next| next.kind) {
            Some(TokenKind::Identifier) if tokens[index + 1].is("global") => push(
                findings,
                "BAMTS-W051",
                token,
                "global augmentation mutates the global declaration environment",
            ),
            Some(TokenKind::Identifier) if tokens[index + 1].is("module") => push(
                findings,
                "BAMTS-W052",
                token,
                "module augmentation mutates another module's declarations",
            ),
            Some(
                TokenKind::KwConst
                | TokenKind::KwLet
                | TokenKind::KwVar
                | TokenKind::KwFunction
                | TokenKind::KwClass,
            ) => push(
                findings,
                "BAMTS-W053",
                token,
                "ambient value declaration has no locally verifiable runtime provider",
            ),
            _ => {}
        }
    }
}

fn find_hygiene(
    tokens: &[SyntaxToken<'_>],
    findings: &mut Vec<(&'static str, TextRange, &'static str)>,
) {
    for (index, token) in tokens.iter().copied().enumerate() {
        if token.identifier()
            && tokens
                .get(index + 1)
                .is_some_and(|next| next.kind == TokenKind::Colon)
            && tokens.get(index + 2).is_some_and(|statement| {
                matches!(
                    statement.kind,
                    TokenKind::KwDo
                        | TokenKind::KwFor
                        | TokenKind::KwIf
                        | TokenKind::KwSwitch
                        | TokenKind::KwTry
                        | TokenKind::KwWhile
                        | TokenKind::KwWith
                )
            })
            && !tokens.iter().enumerate().any(|(use_index, candidate)| {
                matches!(candidate.kind, TokenKind::KwBreak | TokenKind::KwContinue)
                    && tokens
                        .get(use_index + 1)
                        .is_some_and(|label| label.text == token.text)
            })
        {
            push(
                findings,
                "BAMTS-W068",
                token,
                "label is never targeted by break or continue",
            );
        }
    }

    for (index, token) in tokens.iter().copied().enumerate() {
        if matches!(
            token.kind,
            TokenKind::KwConst | TokenKind::KwLet | TokenKind::KwVar
        ) && let Some(name) = tokens
            .get(index + 1)
            .copied()
            .filter(|next| next.identifier())
            && identifier_uses(tokens, name.text) == 1
        {
            push(findings, "BAMTS-W069", name, "local binding is never read");
        }
        if token.kind == TokenKind::KwFunction
            && let Some((start, end)) =
                delimited_after(tokens, index, TokenKind::LParen, TokenKind::RParen)
        {
            for parameter in split_top_level(&tokens[start + 1..end], TokenKind::Comma) {
                if let Some(name) = parameter.iter().copied().find(|item| item.identifier())
                    && identifier_uses(tokens, name.text) == 1
                {
                    push(findings, "BAMTS-W070", name, "parameter is never read");
                }
            }
        }
    }
}

fn find_syntactic_footguns(
    tokens: &[SyntaxToken<'_>],
    findings: &mut Vec<(&'static str, TextRange, &'static str)>,
) {
    for (index, token) in tokens.iter().copied().enumerate() {
        if matches!(token.kind, TokenKind::EqEqEq | TokenKind::BangEqEq)
            && (tokens
                .get(index.wrapping_sub(1))
                .is_some_and(|side| side.is("NaN"))
                || tokens.get(index + 1).is_some_and(|side| side.is("NaN")))
        {
            push(
                findings,
                "BAMTS-W079",
                token,
                "strict comparison with NaN is always false",
            );
        }
    }
}
fn assertion_can_start_at(tokens: &[SyntaxToken<'_>], index: usize) -> bool {
    index == 0
        || tokens.get(index - 1).is_some_and(|previous| {
            matches!(
                previous.kind,
                TokenKind::Arrow
                    | TokenKind::Colon
                    | TokenKind::Comma
                    | TokenKind::Eq
                    | TokenKind::LBracket
                    | TokenKind::LParen
                    | TokenKind::KwReturn
            )
        })
}

fn looks_like_jsx(tokens: &[SyntaxToken<'_>], index: usize) -> bool {
    let Some(next) = tokens.get(index + 1).copied() else {
        return false;
    };
    let fragment = next.kind == TokenKind::GreaterThan;
    if !fragment && !next.identifier() {
        return false;
    }
    let tag = next.text;
    let limit = tokens.len().min(index + 64);
    for cursor in index + 2..limit {
        if tokens[cursor].kind == TokenKind::Semicolon {
            return false;
        }
        if tokens[cursor].kind != TokenKind::GreaterThan {
            continue;
        }
        if tokens
            .get(cursor.wrapping_sub(1))
            .is_some_and(|token| token.kind == TokenKind::Slash)
        {
            return true;
        }
        return tokens[cursor + 1..limit]
            .windows(if fragment { 3 } else { 4 })
            .any(|closing| {
                closing[0].kind == TokenKind::LessThan
                    && closing[1].kind == TokenKind::Slash
                    && if fragment {
                        closing[2].kind == TokenKind::GreaterThan
                    } else {
                        closing[2].text == tag && closing[3].kind == TokenKind::GreaterThan
                    }
            });
    }
    false
}

fn delimited_after(
    tokens: &[SyntaxToken<'_>],
    index: usize,
    open: TokenKind,
    close: TokenKind,
) -> Option<(usize, usize)> {
    let start = tokens[index..]
        .iter()
        .position(|token| token.kind == open)?
        + index;
    let mut depth = 0usize;
    for (offset, token) in tokens[start..].iter().enumerate() {
        if token.kind == open {
            depth += 1;
        } else if token.kind == close {
            depth -= 1;
            if depth == 0 {
                return Some((start, start + offset));
            }
        }
    }
    None
}

fn split_top_level<'tokens, 'source>(
    tokens: &'tokens [SyntaxToken<'source>],
    separator: TokenKind,
) -> Vec<&'tokens [SyntaxToken<'source>]> {
    let mut result = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                depth = depth.saturating_sub(1)
            }
            kind if kind == separator && depth == 0 => {
                result.push(&tokens[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    result.push(&tokens[start..]);
    result
}

fn field_terminator_without_initializer(tokens: &[SyntaxToken<'_>], start: usize) -> bool {
    for token in &tokens[start..] {
        match token.kind {
            TokenKind::Eq => return false,
            TokenKind::Semicolon | TokenKind::RBrace => return true,
            _ => {}
        }
    }
    false
}

fn is_module_source(tokens: &[SyntaxToken<'_>], index: usize) -> bool {
    index > 0
        && (tokens[index - 1].kind == TokenKind::KwFrom
            || tokens[index - 1].kind == TokenKind::KwImport
            || (index > 1
                && tokens[index - 2].kind == TokenKind::KwExport
                && tokens[index - 1].kind == TokenKind::Star))
}

fn extensionless_relative(literal: &str) -> bool {
    let value = literal.trim_matches(['\'', '"']);
    if !(value.starts_with("./") || value.starts_with("../")) {
        return false;
    }
    let last = value.rsplit('/').next().unwrap_or(value);
    !last.contains('.')
}

fn identifier_uses(tokens: &[SyntaxToken<'_>], name: &str) -> usize {
    tokens
        .iter()
        .filter(|token| token.identifier() && token.text == name)
        .count()
}

fn find_catalog_completion(
    source: &SourceFile,
    tokens: &[SyntaxToken<'_>],
    comments: &[SyntaxToken<'_>],
    findings: &mut Vec<(&'static str, TextRange, &'static str)>,
) {
    let anchor = tokens.first().map_or(source.range(), |token| token.range);
    if matches!(
        source.script_kind(),
        ScriptKind::JavaScript | ScriptKind::JavaScriptReact
    ) {
        findings.push((
            "BAMTS-W054",
            anchor,
            "JavaScript source enters the typed program",
        ));
    }
    for comment in comments {
        if comment.text.contains("@type {") || comment.text.contains("@typedef {") {
            push(
                findings,
                "BAMTS-W055",
                *comment,
                "JSDoc comment carries JavaScript type syntax",
            );
        }
        if comment.text.contains("@ts-check") {
            push(
                findings,
                "BAMTS-W057",
                *comment,
                "per-file ts-check directive changes checking policy",
            );
        }
    }
    for window in tokens.windows(7) {
        if window[0].identifier()
            && window[1].kind == TokenKind::Dot
            && window[2].is("prototype")
            && window[3].kind == TokenKind::Dot
            && window[4].identifier()
            && window[5].kind == TokenKind::Eq
            && window[6].is("function")
        {
            push(
                findings,
                "BAMTS-W056",
                window[2],
                "prototype assignment implements class-like behavior",
            );
        }
    }
    visit_statement_list(source.statements(), source.script_kind(), findings);
}

fn visit_statement_list(
    statements: &[Stmt],
    script_kind: ScriptKind,
    findings: &mut Vec<(&'static str, TextRange, &'static str)>,
) {
    let mut reachable = true;
    for statement in statements {
        if !reachable {
            findings.push((
                "BAMTS-W067",
                statement.range(),
                "statement is unreachable after an unconditional transfer",
            ));
        }
        visit_statement(statement, script_kind, findings);
        reachable &= can_complete_normally(statement);
    }
}

fn visit_statement(
    statement: &Stmt,
    script_kind: ScriptKind,
    findings: &mut Vec<(&'static str, TextRange, &'static str)>,
) {
    match statement.data() {
        Statement::Interface(interface) => {
            if matches!(
                script_kind,
                ScriptKind::JavaScript | ScriptKind::JavaScriptReact
            ) {
                findings.push((
                    "BAMTS-W085",
                    statement.range(),
                    "TypeScript-only interface declaration appears in JavaScript",
                ));
            }
            visit_interface(statement.range(), interface, findings);
        }
        Statement::Export(export) => match export {
            ExportDeclaration::All(_) => findings.push((
                "BAMTS-W061",
                statement.range(),
                "wildcard barrel export obscures the public surface",
            )),
            ExportDeclaration::Default(_) => findings.push((
                "BAMTS-W062",
                statement.range(),
                "default export permits arbitrary importer naming",
            )),
            ExportDeclaration::Named(crate::syntax::ExportNamedDeclaration::Declaration(inner)) => {
                visit_statement(inner, script_kind, findings)
            }
            _ => {}
        },
        Statement::Function(function) => {
            visit_function(statement.range(), &function.function, script_kind, findings)
        }
        Statement::Class(class) => {
            for member in &class.members {
                match member.data() {
                    ClassMember::Constructor(constructor) => {
                        flag_parameter_count(
                            member.range(),
                            constructor.parameters.len(),
                            findings,
                        );
                        visit_statement_list(
                            &constructor.body.data().statements,
                            script_kind,
                            findings,
                        );
                    }
                    ClassMember::Method(method) => {
                        visit_function(member.range(), &method.function, script_kind, findings)
                    }
                    _ => {}
                }
            }
        }
        Statement::Variable(variable) => {
            if matches!(
                script_kind,
                ScriptKind::JavaScript | ScriptKind::JavaScriptReact
            ) {
                for declaration in &variable.declarations {
                    if declaration.data().type_annotation.is_some() || declaration.data().definite {
                        findings.push((
                            "BAMTS-W085",
                            declaration.range(),
                            "TypeScript-only declaration syntax appears in JavaScript",
                        ));
                    }
                }
            }
        }
        Statement::Block(block) => {
            visit_statement_list(&block.data().statements, script_kind, findings)
        }
        Statement::If(statement) => {
            visit_statement(&statement.consequent, script_kind, findings);
            if let Some(alternate) = &statement.alternate {
                visit_statement(alternate, script_kind, findings);
            }
        }
        Statement::Switch(statement) => {
            for (index, case) in statement.cases.iter().enumerate() {
                if index + 1 < statement.cases.len()
                    && !case.data().consequent.is_empty()
                    && case
                        .data()
                        .consequent
                        .last()
                        .is_some_and(can_complete_normally)
                {
                    findings.push((
                        "BAMTS-W066",
                        case.range(),
                        "non-empty switch case falls through",
                    ));
                }
                visit_statement_list(&case.data().consequent, script_kind, findings);
            }
        }
        Statement::For(statement) => visit_statement(&statement.body, script_kind, findings),
        Statement::ForIn(statement) => visit_statement(&statement.body, script_kind, findings),
        Statement::ForOf(statement) => visit_statement(&statement.body, script_kind, findings),
        Statement::While(statement) => visit_statement(&statement.body, script_kind, findings),
        Statement::DoWhile(statement) => visit_statement(&statement.body, script_kind, findings),
        Statement::Try(statement) => {
            visit_statement_list(&statement.block.data().statements, script_kind, findings);
            if let Some(handler) = &statement.handler {
                visit_statement_list(
                    &handler.data().body.data().statements,
                    script_kind,
                    findings,
                );
            }
            if let Some(finalizer) = &statement.finalizer {
                visit_statement_list(&finalizer.data().statements, script_kind, findings);
            }
        }
        Statement::Labeled(statement) => visit_statement(&statement.body, script_kind, findings),
        Statement::Declare(inner) => visit_statement(inner, script_kind, findings),
        Statement::TypeAlias(_) | Statement::Enum(_) | Statement::Namespace(_)
            if matches!(
                script_kind,
                ScriptKind::JavaScript | ScriptKind::JavaScriptReact
            ) =>
        {
            findings.push((
                "BAMTS-W085",
                statement.range(),
                "TypeScript-only declaration appears in JavaScript",
            ));
        }
        _ => {}
    }
}

fn visit_interface(
    range: TextRange,
    interface: &InterfaceDeclaration,
    findings: &mut Vec<(&'static str, TextRange, &'static str)>,
) {
    findings.push((
        "BAMTS-W058",
        range,
        "interface leaves the shape open to declaration merging",
    ));
    for member in &interface.members {
        if matches!(member.data(), TypeMember::Method(_)) {
            findings.push((
                "BAMTS-W060",
                member.range(),
                "method signature retains bivariant parameter checking",
            ));
        }
    }
}

fn visit_function(
    range: TextRange,
    function: &FunctionLike,
    script_kind: ScriptKind,
    findings: &mut Vec<(&'static str, TextRange, &'static str)>,
) {
    flag_parameter_count(range, function.parameters.len(), findings);
    for parameter in &function.parameters {
        if parameter
            .data()
            .type_annotation
            .as_ref()
            .is_some_and(|annotation| {
                matches!(annotation.data().type_node.data(), TypeNode::Array(_))
            })
        {
            findings.push((
                "BAMTS-W059",
                parameter.range(),
                "mutable array type crosses a callable boundary",
            ));
        }
        if matches!(
            script_kind,
            ScriptKind::JavaScript | ScriptKind::JavaScriptReact
        ) && parameter.data().type_annotation.is_some()
        {
            findings.push((
                "BAMTS-W085",
                parameter.range(),
                "TypeScript-only parameter type appears in JavaScript",
            ));
        }
    }
    if matches!(
        script_kind,
        ScriptKind::JavaScript | ScriptKind::JavaScriptReact
    ) && (function.return_type.is_some() || function.type_parameters.is_some())
    {
        findings.push((
            "BAMTS-W085",
            range,
            "TypeScript-only function type syntax appears in JavaScript",
        ));
    }
    if let Some(FunctionBody::Block(block)) = &function.body {
        if block_contains_value_return(&block.data().statements)
            && block
                .data()
                .statements
                .last()
                .is_none_or(can_complete_normally)
        {
            findings.push((
                "BAMTS-W065",
                range,
                "function has a reachable path without a returned value",
            ));
        }
        visit_statement_list(&block.data().statements, script_kind, findings);
    }
}

fn flag_parameter_count(
    range: TextRange,
    count: usize,
    findings: &mut Vec<(&'static str, TextRange, &'static str)>,
) {
    if count >= 5 {
        findings.push((
            "BAMTS-W064",
            range,
            "callable has five or more positional parameters",
        ));
    }
}

fn block_contains_value_return(statements: &[Stmt]) -> bool {
    statements.iter().any(|statement| match statement.data() {
        Statement::Return(ret) => ret.argument.is_some(),
        Statement::Block(block) => block_contains_value_return(&block.data().statements),
        Statement::If(statement) => {
            block_contains_value_return(std::slice::from_ref(statement.consequent.as_ref()))
                || statement.alternate.as_ref().is_some_and(|alternate| {
                    block_contains_value_return(std::slice::from_ref(alternate.as_ref()))
                })
        }
        Statement::Switch(statement) => statement
            .cases
            .iter()
            .any(|case| block_contains_value_return(&case.data().consequent)),
        _ => false,
    })
}

fn can_complete_normally(statement: &Stmt) -> bool {
    match statement.data() {
        Statement::Return(_)
        | Statement::Throw(_)
        | Statement::Break(_)
        | Statement::Continue(_) => false,
        Statement::Block(block) => block
            .data()
            .statements
            .last()
            .is_none_or(can_complete_normally),
        Statement::If(statement) => statement.alternate.as_ref().is_none_or(|alternate| {
            can_complete_normally(&statement.consequent) || can_complete_normally(alternate)
        }),
        Statement::Try(statement) => {
            if statement.finalizer.as_ref().is_some_and(|block| {
                !block
                    .data()
                    .statements
                    .last()
                    .is_none_or(can_complete_normally)
            }) {
                return false;
            }
            let try_completes = statement
                .block
                .data()
                .statements
                .last()
                .is_none_or(can_complete_normally);
            let catch_completes = statement.handler.as_ref().is_some_and(|handler| {
                handler
                    .data()
                    .body
                    .data()
                    .statements
                    .last()
                    .is_none_or(can_complete_normally)
            });
            try_completes || catch_completes
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::analyze;
    use crate::{
        diagnostic::DiagnosticSeverity,
        lint::{LintLevel, LintOverride, LintProfile, LintTable, rule_by_code},
        parser, scanner,
        source::{ScriptKind, SourceId, SourceText},
    };
    use std::sync::Arc;

    fn codes(source: &str, kind: ScriptKind) -> Vec<&'static str> {
        let parsed = parser::parse(scanner::scan(
            SourceId::new(0),
            kind,
            Arc::new(SourceText::new(source)),
        ));
        let mut levels = LintTable::new(LintProfile::Pedantic);
        levels
            .apply_cli(["BAMTS-W033", "BAMTS-W034"].into_iter().map(|code| {
                LintOverride::rule(
                    rule_by_code(code).expect("test rule is registered").id(),
                    LintLevel::Warn,
                    "syntax rule test",
                )
            }))
            .expect("test overrides cannot lower forbid rules");
        analyze(parsed.product(), &levels)
            .iter()
            .map(|diagnostic| diagnostic.code().as_str())
            .collect()
    }

    fn fires(code: &str, trigger: &str, safe: &str) {
        assert!(
            codes(trigger, ScriptKind::TypeScript).contains(&code),
            "{code} did not fire for {trigger:?}"
        );
        assert!(
            !codes(safe, ScriptKind::TypeScript).contains(&code),
            "{code} fired for {safe:?}"
        );
    }

    macro_rules! rule_test {
        ($name:ident, $code:literal, $trigger:literal, $safe:literal) => {
            #[test]
            fn $name() {
                fires($code, $trigger, $safe);
            }
        };
    }

    rule_test!(
        w017_explicit_any,
        "BAMTS-W017",
        "let value: any = 1; value;",
        "let value: unknown = 1; value;"
    );
    rule_test!(
        w020_double_assertion,
        "BAMTS-W020",
        "value as unknown as number;",
        "value as number;"
    );
    rule_test!(w021_non_null, "BAMTS-W021", "value!.name;", "value.name;");
    rule_test!(
        w022_definite_assignment,
        "BAMTS-W022",
        "class C { value!: string; }",
        "class C { value: string = ''; }"
    );
    rule_test!(
        w023_suppression,
        "BAMTS-W023",
        "// @ts-ignore\nvalue;",
        "// ordinary comment\nvalue;"
    );
    rule_test!(
        w024_runtime_namespace,
        "BAMTS-W024",
        "namespace N { export const x = 1; }",
        "namespace N { export interface X {} }"
    );
    rule_test!(
        w025_parameter_property,
        "BAMTS-W025",
        "class C { constructor(public x: number) {} }",
        "class C { constructor(x: number) {} }"
    );
    rule_test!(
        w026_decorator,
        "BAMTS-W026",
        "@sealed class C {}",
        "class C {}"
    );
    rule_test!(
        w027_angle_assertion,
        "BAMTS-W027",
        "const n = <number>value; n;",
        "const n = value as number; n;"
    );
    rule_test!(
        w030_import_equals,
        "BAMTS-W030",
        "import fs = require('fs'); fs;",
        "import * as fs from 'fs'; fs;"
    );
    rule_test!(
        w033_commonjs_esm,
        "BAMTS-W033",
        "export const x = require('x');",
        "const x = require('x'); x;"
    );
    rule_test!(
        w034_implicit_script,
        "BAMTS-W034",
        "const x = 1; x;",
        "export const x = 1;"
    );
    rule_test!(
        w036_extensionless_import,
        "BAMTS-W036",
        "import { x } from './util'; x;",
        "import { x } from './util.js'; x;"
    );
    rule_test!(
        w039_uninitialized_field,
        "BAMTS-W039",
        "class C { value: string; }",
        "class C { value: string = ''; }"
    );
    rule_test!(
        w042_private_field,
        "BAMTS-W042",
        "class C { private value = 1; }",
        "class C { #value = 1; }"
    );
    rule_test!(
        w043_runtime_enum,
        "BAMTS-W043",
        "enum Color { Red }",
        "const enum Color { Red }"
    );
    rule_test!(
        w044_const_enum,
        "BAMTS-W044",
        "const enum Code { Ok = 200 }",
        "enum Code { Ok = 200 }"
    );
    rule_test!(
        w046_heterogeneous_enum,
        "BAMTS-W046",
        "enum Answer { No = 0, Yes = 'YES' }",
        "enum Answer { No = 0, Yes = 1 }"
    );
    rule_test!(
        w047_computed_enum,
        "BAMTS-W047",
        "enum E { X = getValue() }",
        "enum E { X = 1 }"
    );
    rule_test!(
        w049_interface_merge,
        "BAMTS-W049",
        "interface Box { x: number } interface Box { y: number }",
        "interface Box { x: number } interface Bag { y: number }"
    );
    rule_test!(
        w050_namespace_value_merge,
        "BAMTS-W050",
        "function f() {} namespace f { export const x = 1; }",
        "function f() {} namespace helpers { export const x = 1; }"
    );
    rule_test!(
        w051_global_augmentation,
        "BAMTS-W051",
        "declare global { interface Window { x: number } }",
        "declare namespace Local { interface X {} }"
    );
    rule_test!(
        w052_module_augmentation,
        "BAMTS-W052",
        "declare module 'lib' { interface X { y: number } }",
        "namespace lib { interface X { y: number } }"
    );
    rule_test!(
        w053_ambient_value,
        "BAMTS-W053",
        "declare const injected: string;",
        "declare interface Injected { value: string }"
    );
    rule_test!(
        w068_unused_label,
        "BAMTS-W068",
        "outer: while (true) { break; }",
        "outer: while (true) { break outer; }"
    );
    rule_test!(
        w069_unused_local,
        "BAMTS-W069",
        "function f() { const x = 1; } f();",
        "function f() { const x = 1; return x; } f();"
    );
    rule_test!(
        w070_unused_parameter,
        "BAMTS-W070",
        "function f(value: number) { return 1; } f(1);",
        "function f(value: number) { return value; } f(1);"
    );
    rule_test!(
        w079_nan_comparison,
        "BAMTS-W079",
        "if (value === NaN) {}",
        "if (Number.isNaN(value)) {}"
    );

    #[test]
    fn w029_jsx_transform_requires_react_source_kind() {
        assert!(
            codes("const el = <Widget />;", ScriptKind::TypeScriptReact).contains(&"BAMTS-W029")
        );
        assert!(
            !codes("const n = value as number;", ScriptKind::TypeScript).contains(&"BAMTS-W029")
        );
        assert!(!codes("if (a < b) {}", ScriptKind::TypeScriptReact).contains(&"BAMTS-W029"));
    }

    #[test]
    fn javascript_is_limited_to_footgun_and_hygiene_warnings() {
        let diagnostics = codes("let value: any; value === NaN;", ScriptKind::JavaScript);
        assert!(diagnostics.contains(&"BAMTS-W079"));
        assert!(!diagnostics.contains(&"BAMTS-W017"));
    }

    #[test]
    fn double_assertion_does_not_cross_expression_boundaries() {
        let diagnostics = codes(
            "const a = value as unknown; const b = other as string;",
            ScriptKind::TypeScript,
        );
        assert!(!diagnostics.contains(&"BAMTS-W020"));
    }

    #[test]
    fn typed_local_inside_method_is_not_a_class_field() {
        let diagnostics = codes(
            "class C { method() { let value: string; value = ''; } }",
            ScriptKind::TypeScript,
        );
        assert!(!diagnostics.contains(&"BAMTS-W039"));
    }

    #[test]
    fn strict_denies_runtime_enum_but_keeps_const_enum_as_a_warning() {
        let parsed = parser::parse(scanner::scan(
            SourceId::new(0),
            ScriptKind::TypeScript,
            Arc::new(SourceText::new(
                "enum Runtime { Value } const enum Inlined { Value }",
            )),
        ));
        let diagnostics = analyze(parsed.product(), &LintTable::new(LintProfile::Strict));
        let runtime = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code().as_str() == "BAMTS-W043")
            .expect("runtime enum must be diagnosed");
        let inlined = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code().as_str() == "BAMTS-W044")
            .expect("const enum must be diagnosed");
        assert_eq!(runtime.severity(), DiagnosticSeverity::Error);
        assert_eq!(inlined.severity(), DiagnosticSeverity::Warning);
    }
}
