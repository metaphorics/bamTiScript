use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    checker::{SymbolId, SymbolKind},
    diagnostic::DiagnosticSeverity,
    scanner,
    source::{ScriptKind, SourceId, SourceText, TextRange, Utf16Pos},
    syntax::{Token, TokenKind},
};

use super::{DocumentSnapshot, ServiceError, ServiceState, filesystem::FileSystem};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
    pub name: String,
    pub kind: SymbolKind,
    pub replacement: TextRange,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Location {
    pub path: PathBuf,
    pub range: TextRange,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentEdit {
    pub path: PathBuf,
    pub range: TextRange,
    pub replacement: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenameEdit {
    pub edits: Vec<DocumentEdit>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenameResult {
    pub symbol: String,
    pub edit: RenameEdit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticEntry {
    pub path: PathBuf,
    pub range: TextRange,
    pub code: &'static str,
    pub severity: DiagnosticSeverity,
    pub message: &'static str,
}

impl<F: FileSystem> ServiceState<F> {
    pub fn completions(
        &mut self,
        path: impl AsRef<Path>,
        position: Utf16Pos,
    ) -> Result<Vec<Completion>, ServiceError> {
        let document = self.ensure_document(path.as_ref())?;
        validate_position(&document, position)?;
        let (prefix, replacement) = completion_prefix(&document, position);
        let mut seen = BTreeSet::new();
        let mut completions = document
            .semantic()
            .symbols()
            .iter()
            .filter(|symbol| symbol.name().starts_with(prefix.as_str()))
            .filter(|symbol| seen.insert(symbol.name().to_owned()))
            .map(|symbol| Completion {
                name: symbol.name().to_owned(),
                kind: symbol.kind(),
                replacement,
            })
            .collect::<Vec<_>>();
        completions.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(completions)
    }

    pub fn definition(
        &mut self,
        path: impl AsRef<Path>,
        position: Utf16Pos,
    ) -> Result<Option<Location>, ServiceError> {
        let document = self.ensure_document(path.as_ref())?;
        let Some(symbol) = symbol_at(&document, position)? else {
            return Ok(None);
        };
        Ok(Some(Location {
            path: document.path().to_path_buf(),
            range: document.semantic().symbol(symbol).range(),
        }))
    }

    pub fn references(
        &mut self,
        path: impl AsRef<Path>,
        position: Utf16Pos,
    ) -> Result<Vec<Location>, ServiceError> {
        let document = self.ensure_document(path.as_ref())?;
        let Some(symbol) = symbol_at(&document, position)? else {
            return Ok(Vec::new());
        };
        Ok(reference_locations(&document, symbol))
    }

    pub fn rename(
        &mut self,
        path: impl AsRef<Path>,
        position: Utf16Pos,
        new_name: &str,
    ) -> Result<RenameResult, ServiceError> {
        if !is_rename_identifier(new_name) {
            return Err(ServiceError::InvalidRename(format!(
                "rename target is not an identifier: {new_name}"
            )));
        }
        let document = self.ensure_document(path.as_ref())?;
        let symbol = symbol_at(&document, position)?.ok_or(ServiceError::RenameUnavailable)?;
        let target = document.semantic().symbol(symbol);
        let old_name = target.name();
        if document
            .semantic()
            .symbols()
            .iter()
            .enumerate()
            .any(|(index, candidate)| {
                index != symbol_index(symbol)
                    && candidate.scope() == target.scope()
                    && candidate.name() == new_name
            })
        {
            return Err(ServiceError::InvalidRename(format!(
                "rename would conflict with existing symbol `{new_name}`"
            )));
        }
        let edits = reference_locations(&document, symbol)
            .into_iter()
            .map(|location| DocumentEdit {
                path: location.path,
                range: location.range,
                replacement: new_name.to_owned(),
            })
            .collect();
        Ok(RenameResult {
            symbol: old_name.to_owned(),
            edit: RenameEdit { edits },
        })
    }

    pub fn diagnostics(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<Vec<DiagnosticEntry>, ServiceError> {
        let document = self.ensure_document(path.as_ref())?;
        Ok(document
            .diagnostics()
            .iter()
            .map(|diagnostic| DiagnosticEntry {
                path: document.path().to_path_buf(),
                range: diagnostic.range(),
                code: diagnostic.code().as_str(),
                severity: diagnostic.severity(),
                message: diagnostic.message(),
            })
            .collect())
    }
}

fn validate_position(document: &DocumentSnapshot, position: Utf16Pos) -> Result<(), ServiceError> {
    document
        .source()
        .source_text()
        .utf16_to_byte(position)
        .map(|_| ())
        .map_err(|_| ServiceError::InvalidPosition {
            path: document.path().to_path_buf(),
            offset: position.get(),
        })
}

fn completion_prefix(document: &DocumentSnapshot, position: Utf16Pos) -> (String, TextRange) {
    let Some(token) = token_at(document, position) else {
        return (
            String::new(),
            TextRange::new(position, position).expect("ordered empty range"),
        );
    };
    if identifier_text(document, token).is_none() {
        return (
            String::new(),
            TextRange::new(position, position).expect("ordered empty range"),
        );
    }
    if position < token.range().start() || position > token.range().end() {
        return (
            String::new(),
            TextRange::new(position, position).expect("ordered empty range"),
        );
    }
    let end = document
        .source()
        .source_text()
        .utf16_to_byte(position)
        .expect("position validated");
    let start = document
        .source()
        .source_text()
        .utf16_to_byte(token.range().start())
        .expect("token boundary");
    let prefix = document.source().source_text().as_str()[start..end].to_owned();
    (prefix, token.range())
}

fn symbol_at(
    document: &DocumentSnapshot,
    position: Utf16Pos,
) -> Result<Option<SymbolId>, ServiceError> {
    validate_position(document, position)?;
    let Some(token) = token_at(document, position) else {
        return Ok(None);
    };
    let Some(name) = identifier_text(document, token) else {
        return Ok(None);
    };
    let model = document.semantic();

    if let Some((index, _)) = model
        .symbols()
        .iter()
        .enumerate()
        .find(|(_, symbol)| symbol.name() == name && symbol.range() == token.range())
    {
        return Ok(Some(SymbolId::new(index as u32)));
    }

    let mut matching = model
        .symbols()
        .iter()
        .enumerate()
        .filter(|(_, symbol)| symbol.name() == name);
    let first = matching
        .next()
        .map(|(index, _)| SymbolId::new(index as u32));
    if matching.next().is_none() {
        Ok(first)
    } else {
        Ok(None)
    }
}

fn reference_locations(document: &DocumentSnapshot, symbol: SymbolId) -> Vec<Location> {
    let model = document.semantic();
    let declaration = model.symbol(symbol);
    let semantic_reference_count = model
        .references()
        .filter(|(_, target)| *target == symbol)
        .count();
    let mut candidates = document
        .source()
        .tokens()
        .iter()
        .filter(|token| identifier_text(document, token).as_deref() == Some(declaration.name()))
        .map(Token::range)
        .collect::<Vec<_>>();

    // The checker owns symbol identity. Lexical ranges are used only when their
    // cardinality exactly matches its declaration-plus-reference fact; otherwise
    // the service returns the declaration rather than inventing references.
    if candidates.len() != semantic_reference_count + 1 {
        candidates.clear();
        candidates.push(declaration.range());
    }
    candidates.sort_by_key(|range| (range.start(), range.end()));
    candidates.dedup();
    candidates
        .into_iter()
        .map(|range| Location {
            path: document.path().to_path_buf(),
            range,
        })
        .collect()
}

fn token_at(document: &DocumentSnapshot, position: Utf16Pos) -> Option<&Token> {
    let tokens = document.source().tokens();
    tokens
        .iter()
        .find(|token| {
            !token.is_missing()
                && token.range().start() < position
                && position < token.range().end()
        })
        .or_else(|| {
            tokens.iter().find(|token| {
                !token.is_missing()
                    && token.range().start() == position
                    && identifier_text(document, token).is_some()
            })
        })
        .or_else(|| {
            tokens.iter().find(|token| {
                !token.is_missing()
                    && token.range().end() == position
                    && identifier_text(document, token).is_some()
            })
        })
        .or_else(|| {
            tokens.iter().find(|token| {
                !token.is_missing()
                    && token.range().start() <= position
                    && position <= token.range().end()
            })
        })
}

fn identifier_text<'a>(document: &'a DocumentSnapshot, token: &'a Token) -> Option<String> {
    let text = document.source().identifier_text(token)?.into_owned();
    is_identifier(&text).then_some(text)
}

fn is_identifier(text: &str) -> bool {
    let mut characters = text.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first == '_' || first == '$' || first.is_alphabetic())
        && characters
            .all(|character| character == '_' || character == '$' || character.is_alphanumeric())
}

fn is_rename_identifier(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let Ok(source) = SourceText::new(text) else {
        return false;
    };
    let scanned = scanner::scan(
        SourceId::new(u32::MAX),
        ScriptKind::TypeScript,
        Arc::new(source),
    );
    scanned.diagnostics().is_empty()
        && matches!(scanned.product().tokens(), [token] if token.kind() == TokenKind::Identifier)
}

const fn symbol_index(symbol: SymbolId) -> usize {
    symbol.get() as usize
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::service::{ServiceError, ServiceState, filesystem::OsFileSystem};

    fn state(source: &str) -> (PathBuf, ServiceState<OsFileSystem>) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "bamts-language-service-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("create root");
        let mut state = ServiceState::new(OsFileSystem::new(&root).expect("filesystem"));
        state.open("a.ts", source, 1).expect("open");
        (root, state)
    }

    #[test]
    fn completion_navigation_references_and_rename_share_checker_snapshot() {
        let (root, mut state) = state("const answer = 1;\nans\nanswer;\n");
        let completions = state
            .completions("a.ts", Utf16Pos::new(21))
            .expect("completions");
        assert_eq!(
            completions
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["answer"]
        );

        let definition = state
            .definition("a.ts", Utf16Pos::new(28))
            .expect("definition")
            .expect("symbol");
        assert_eq!(definition.range.start(), Utf16Pos::new(6));
        let references = state
            .references("a.ts", Utf16Pos::new(28))
            .expect("references");
        assert_eq!(references.len(), 2);
        let rename = state
            .rename("a.ts", Utf16Pos::new(28), "result")
            .expect("rename");
        assert_eq!(rename.edit.edits.len(), 2);
        assert!(
            rename
                .edit
                .edits
                .iter()
                .all(|edit| edit.replacement == "result")
        );
        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn rename_ignores_unrelated_nested_shadowing_declarations() {
        let (root, mut state) =
            state("const outer = 1; { const replacement = 2; replacement; } outer;");
        let rename = state
            .rename("a.ts", Utf16Pos::new(58), "replacement")
            .expect("nested declaration is outside the target scope");
        assert_eq!(rename.edit.edits.len(), 2);
        assert!(
            rename
                .edit
                .edits
                .iter()
                .all(|edit| edit.replacement == "replacement")
        );
        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn diagnostics_and_failure_paths_are_public_data() {
        let (root, mut state) = state("const value: number = 'wrong';");
        assert!(
            state
                .diagnostics("a.ts")
                .expect("diagnostics")
                .iter()
                .any(|item| item.code == "BAMTS-C004")
        );
        assert!(matches!(
            state.definition("a.ts", Utf16Pos::new(10_000)),
            Err(ServiceError::InvalidPosition { .. })
        ));
        assert!(matches!(
            state.rename("a.ts", Utf16Pos::new(6), "not-valid!"),
            Err(ServiceError::InvalidRename(_))
        ));
        fs::remove_dir_all(root).expect("remove root");
    }
}
