//! Incremental language-service state over the compiler's parser and checker products.
//!
//! A document update constructs one immutable [`DocumentSnapshot`]. Every synchronous
//! and asynchronous query borrows that same snapshot; the service never reparses or
//! rechecks inside an individual language operation.

pub mod r#async;
pub mod filesystem;
pub mod language_service;
pub mod sync;

use bamts_cancel::CancellationToken;
use std::{
    collections::BTreeMap,
    fmt, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    checker::{self, SemanticModel},
    diagnostic::{Diagnostic, Recovered},
    lint::{LintProfile, LintTable},
    parser, scanner,
    source::{ScriptKind, SourceId, SourcePositionError, SourceText},
    syntax::SourceFile,
};

use filesystem::{FileMetadata, FileSystem, FileSystemError};

pub use language_service::{
    Completion, DiagnosticEntry, DocumentEdit, Location, QuickInfo, QuickInfoKind, RenameEdit,
    RenameResult,
};

/// A public service request failure. Compiler diagnostics remain ordinary query data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceError {
    FileSystem(FileSystemError),
    Source(SourcePositionError),
    DocumentNotOpen(PathBuf),
    DocumentNotFound(PathBuf),
    VersionOutOfOrder {
        path: PathBuf,
        current: u64,
        proposed: u64,
    },
    InvalidPosition {
        path: PathBuf,
        offset: usize,
    },
    InvalidRename(String),
    RenameUnavailable,
    Cancelled,
    Backpressure,
    LockPoisoned,
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => error.fmt(formatter),
            Self::FileSystem(error) => error.fmt(formatter),
            Self::DocumentNotOpen(path) => {
                write!(formatter, "document is not open: {}", path.display())
            }
            Self::DocumentNotFound(path) => {
                write!(formatter, "document was not found: {}", path.display())
            }
            Self::VersionOutOfOrder {
                path,
                current,
                proposed,
            } => write!(
                formatter,
                "document version must increase for {}: current={current}, proposed={proposed}",
                path.display()
            ),
            Self::InvalidPosition { path, offset } => {
                write!(
                    formatter,
                    "invalid UTF-16 position {offset} in {}",
                    path.display()
                )
            }
            Self::InvalidRename(message) => formatter.write_str(message),
            Self::RenameUnavailable => {
                formatter.write_str("no renameable symbol at the requested position")
            }
            Self::Cancelled => formatter.write_str("service request was cancelled"),
            Self::Backpressure => formatter.write_str("service request queue is full"),
            Self::LockPoisoned => formatter.write_str("service state lock is poisoned"),
        }
    }
}

impl std::error::Error for ServiceError {}

impl From<FileSystemError> for ServiceError {
    fn from(error: FileSystemError) -> Self {
        Self::FileSystem(error)
    }
}

impl From<SourcePositionError> for ServiceError {
    fn from(error: SourcePositionError) -> Self {
        Self::Source(error)
    }
}

/// One immutable compiler/checker product for one version of a source file.
#[derive(Clone)]
pub struct DocumentSnapshot {
    path: PathBuf,
    version: u64,
    open: bool,
    parsed: Arc<Recovered<SourceFile>>,
    semantic: Arc<Recovered<SemanticModel>>,
    diagnostics: Arc<[Diagnostic]>,
    disk_metadata: Option<FileMetadata>,
}

impl DocumentSnapshot {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }
    #[must_use]
    pub fn source(&self) -> &SourceFile {
        self.parsed.product()
    }
    #[must_use]
    pub fn semantic(&self) -> &SemanticModel {
        self.semantic.product()
    }
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
}

/// A coherent, immutable view of every source known to the service.
#[derive(Clone, Default)]
pub struct ServiceSnapshot {
    documents: BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
}

impl ServiceSnapshot {
    #[must_use]
    pub fn document(&self, path: &Path) -> Option<&DocumentSnapshot> {
        self.documents.get(path).map(Arc::as_ref)
    }

    pub fn documents(&self) -> impl Iterator<Item = &DocumentSnapshot> {
        self.documents.values().map(Arc::as_ref)
    }
}

/// Mutable project state. Mutations replace whole document snapshots atomically.
pub struct ServiceState<F: FileSystem> {
    pub(crate) filesystem: F,
    pub(crate) documents: BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    next_source_id: u32,
}

impl<F: FileSystem> ServiceState<F> {
    #[must_use]
    pub fn new(filesystem: F) -> Self {
        Self {
            filesystem,
            documents: BTreeMap::new(),
            next_source_id: 0,
        }
    }

    pub fn open(
        &mut self,
        path: impl AsRef<Path>,
        text: impl Into<Arc<str>>,
        version: u64,
    ) -> Result<Arc<DocumentSnapshot>, ServiceError> {
        self.open_with_cancel(path, text, version, &CancellationToken::new())
    }

    pub fn open_with_cancel(
        &mut self,
        path: impl AsRef<Path>,
        text: impl Into<Arc<str>>,
        version: u64,
        cancel: &CancellationToken,
    ) -> Result<Arc<DocumentSnapshot>, ServiceError> {
        cancel.check().map_err(|_| ServiceError::Cancelled)?;
        let path = self.filesystem.normalize(path.as_ref())?;
        if let Some(current) = self.documents.get(&path).filter(|document| document.open)
            && version <= current.version
        {
            return Err(ServiceError::VersionOutOfOrder {
                path,
                current: current.version,
                proposed: version,
            });
        }
        let source_id = self.source_id_for(&path);
        let snapshot = Arc::new(build_snapshot_with_cancel(
            path.clone(),
            source_id,
            text.into(),
            version,
            true,
            None,
            cancel,
        )?);
        self.commit_new_document(path, snapshot, cancel)
    }

    pub fn update(
        &mut self,
        path: impl AsRef<Path>,
        text: impl Into<Arc<str>>,
        version: u64,
    ) -> Result<Arc<DocumentSnapshot>, ServiceError> {
        self.update_with_cancel(path, text, version, &CancellationToken::new())
    }

    pub fn update_with_cancel(
        &mut self,
        path: impl AsRef<Path>,
        text: impl Into<Arc<str>>,
        version: u64,
        cancel: &CancellationToken,
    ) -> Result<Arc<DocumentSnapshot>, ServiceError> {
        cancel.check().map_err(|_| ServiceError::Cancelled)?;
        let path = self.filesystem.normalize(path.as_ref())?;
        let current = self
            .documents
            .get(&path)
            .filter(|document| document.open)
            .ok_or_else(|| ServiceError::DocumentNotOpen(path.clone()))?;
        if version <= current.version {
            return Err(ServiceError::VersionOutOfOrder {
                path,
                current: current.version,
                proposed: version,
            });
        }
        let source_id = current.source().source_id();
        let snapshot = Arc::new(build_snapshot_with_cancel(
            path.clone(),
            source_id,
            text.into(),
            version,
            true,
            None,
            cancel,
        )?);
        self.commit_replacement_document(path, snapshot, cancel)
    }

    pub fn close(&mut self, path: impl AsRef<Path>) -> Result<(), ServiceError> {
        self.close_with_cancel(path, &CancellationToken::new())
    }

    pub fn close_with_cancel(
        &mut self,
        path: impl AsRef<Path>,
        cancel: &CancellationToken,
    ) -> Result<(), ServiceError> {
        cancel.check().map_err(|_| ServiceError::Cancelled)?;
        let path = self.filesystem.normalize(path.as_ref())?;
        let current = self
            .documents
            .get(&path)
            .filter(|document| document.open)
            .ok_or_else(|| ServiceError::DocumentNotOpen(path.clone()))?;
        let source_id = current.source().source_id();
        match self.filesystem.read(&path) {
            Ok(text) => {
                let metadata = self.filesystem.metadata(&path).ok();
                let snapshot = build_snapshot_with_cancel(
                    path.clone(),
                    source_id,
                    Arc::<str>::from(text),
                    0,
                    false,
                    metadata,
                    cancel,
                )?;
                self.commit_replacement_document(path, Arc::new(snapshot), cancel)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                cancel.check().map_err(|_| ServiceError::Cancelled)?;
                self.documents.remove(&path);
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    #[must_use]
    pub fn snapshot(&self) -> ServiceSnapshot {
        ServiceSnapshot {
            documents: self.documents.clone(),
        }
    }

    pub(crate) fn ensure_document(
        &mut self,
        path: &Path,
    ) -> Result<Arc<DocumentSnapshot>, ServiceError> {
        self.ensure_document_with_cancel(path, &CancellationToken::new())
    }

    pub(crate) fn ensure_document_with_cancel(
        &mut self,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<Arc<DocumentSnapshot>, ServiceError> {
        cancel.check().map_err(|_| ServiceError::Cancelled)?;
        let path = self.filesystem.normalize(path)?;
        if let Some(document) = self.documents.get(&path).filter(|document| document.open) {
            return Ok(Arc::clone(document));
        }

        let metadata = self.filesystem.metadata(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                ServiceError::DocumentNotFound(path.clone())
            } else {
                error.into()
            }
        })?;
        if let Some(document) = self.documents.get(&path)
            && document.disk_metadata == Some(metadata)
        {
            return Ok(Arc::clone(document));
        }

        let text = self.filesystem.read(&path)?;
        let source_id = self.source_id_for(&path);
        let snapshot = Arc::new(build_snapshot_with_cancel(
            path.clone(),
            source_id,
            Arc::<str>::from(text),
            0,
            false,
            Some(metadata),
            cancel,
        )?);
        self.commit_new_document(path, snapshot, cancel)
    }

    fn commit_new_document(
        &mut self,
        path: PathBuf,
        snapshot: Arc<DocumentSnapshot>,
        cancel: &CancellationToken,
    ) -> Result<Arc<DocumentSnapshot>, ServiceError> {
        cancel.check().map_err(|_| ServiceError::Cancelled)?;
        self.commit_source_id(&path);
        self.documents.insert(path, Arc::clone(&snapshot));
        Ok(snapshot)
    }

    fn commit_replacement_document(
        &mut self,
        path: PathBuf,
        snapshot: Arc<DocumentSnapshot>,
        cancel: &CancellationToken,
    ) -> Result<Arc<DocumentSnapshot>, ServiceError> {
        cancel.check().map_err(|_| ServiceError::Cancelled)?;
        self.documents.insert(path, Arc::clone(&snapshot));
        Ok(snapshot)
    }

    fn source_id_for(&self, path: &Path) -> SourceId {
        self.documents.get(path).map_or_else(
            || SourceId::new(self.next_source_id),
            |document| document.source().source_id(),
        )
    }

    fn commit_source_id(&mut self, path: &Path) {
        if !self.documents.contains_key(path) {
            self.next_source_id = self
                .next_source_id
                .checked_add(1)
                .expect("source id space exhausted");
        }
    }
}

impl<F: FileSystem> ServiceState<F> {
    pub fn completions_with_cancel(
        &mut self,
        path: impl AsRef<Path>,
        position: crate::source::Utf16Pos,
        cancel: &CancellationToken,
    ) -> Result<Vec<Completion>, ServiceError> {
        let path = path.as_ref();
        self.ensure_document_with_cancel(path, cancel)?;
        cancel.check().map_err(|_| ServiceError::Cancelled)?;
        self.completions(path, position)
    }

    pub fn definition_with_cancel(
        &mut self,
        path: impl AsRef<Path>,
        position: crate::source::Utf16Pos,
        cancel: &CancellationToken,
    ) -> Result<Option<Location>, ServiceError> {
        let path = path.as_ref();
        self.ensure_document_with_cancel(path, cancel)?;
        cancel.check().map_err(|_| ServiceError::Cancelled)?;
        self.definition(path, position)
    }

    pub fn references_with_cancel(
        &mut self,
        path: impl AsRef<Path>,
        position: crate::source::Utf16Pos,
        cancel: &CancellationToken,
    ) -> Result<Vec<Location>, ServiceError> {
        let path = path.as_ref();
        self.ensure_document_with_cancel(path, cancel)?;
        cancel.check().map_err(|_| ServiceError::Cancelled)?;
        self.references(path, position)
    }

    pub fn quick_info_with_cancel(
        &mut self,
        path: impl AsRef<Path>,
        position: crate::source::Utf16Pos,
        cancel: &CancellationToken,
    ) -> Result<Option<QuickInfo>, ServiceError> {
        let path = path.as_ref();
        self.ensure_document_with_cancel(path, cancel)?;
        cancel.check().map_err(|_| ServiceError::Cancelled)?;
        self.quick_info(path, position)
    }

    pub fn rename_with_cancel(
        &mut self,
        path: impl AsRef<Path>,
        position: crate::source::Utf16Pos,
        new_name: &str,
        cancel: &CancellationToken,
    ) -> Result<RenameResult, ServiceError> {
        let path = path.as_ref();
        self.ensure_document_with_cancel(path, cancel)?;
        cancel.check().map_err(|_| ServiceError::Cancelled)?;
        self.rename(path, position, new_name)
    }

    pub fn diagnostics_with_cancel(
        &mut self,
        path: impl AsRef<Path>,
        cancel: &CancellationToken,
    ) -> Result<Vec<DiagnosticEntry>, ServiceError> {
        let path = path.as_ref();
        self.ensure_document_with_cancel(path, cancel)?;
        cancel.check().map_err(|_| ServiceError::Cancelled)?;
        self.diagnostics(path)
    }
}

fn build_snapshot_with_cancel(
    path: PathBuf,
    source_id: SourceId,
    text: Arc<str>,
    version: u64,
    open: bool,
    disk_metadata: Option<FileMetadata>,
    cancel: &CancellationToken,
) -> Result<DocumentSnapshot, ServiceError> {
    cancel.check().map_err(|_| ServiceError::Cancelled)?;
    let declaration_file = is_declaration_file(&path);
    let source = Arc::new(SourceText::from_arc(text)?.with_declaration_file(declaration_file));
    let scanned = scanner::scan_with_cancel(source_id, script_kind(&path), source, cancel.clone())
        .map_err(|_| ServiceError::Cancelled)?;
    let parsed =
        parser::parse_with_cancel(scanned, cancel.clone()).map_err(|_| ServiceError::Cancelled)?;
    let semantic = checker::check_source_with_lints_with_cancel(
        parsed.product(),
        &LintTable::new(LintProfile::Default),
        cancel.clone(),
    )
    .map_err(|_| ServiceError::Cancelled)?;
    let diagnostics = Arc::from(semantic.diagnostics().to_vec());
    Ok(DocumentSnapshot {
        path,
        version,
        open,
        parsed: Arc::new(parsed),
        semantic: Arc::new(semantic),
        diagnostics,
        disk_metadata,
    })
}

fn is_declaration_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.ends_with(".d.ts") || name.ends_with(".d.mts") || name.ends_with(".d.cts")
}

fn script_kind(path: &Path) -> ScriptKind {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("js" | "mjs" | "cjs") => ScriptKind::JavaScript,
        Some("jsx") => ScriptKind::JavaScriptReact,
        Some("tsx") => ScriptKind::TypeScriptReact,
        Some("json") => ScriptKind::Json,
        _ => ScriptKind::TypeScript,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{CancellationToken, ServiceError, ServiceState, build_snapshot_with_cancel};
    use crate::{service::filesystem::OsFileSystem, source::SourceId};

    fn state() -> (std::path::PathBuf, ServiceState<OsFileSystem>) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "bamts-service-state-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("create root");
        let state = ServiceState::new(OsFileSystem::new(&root).expect("filesystem"));
        (root, state)
    }

    #[test]
    fn updates_replace_snapshots_and_reject_stale_versions() {
        let (root, mut state) = state();
        let first = state
            .open("a.ts", Arc::<str>::from("const a = 1;"), 1)
            .expect("open");
        let frozen = state.snapshot();
        let second = state
            .update("a.ts", Arc::<str>::from("const b = 2;"), 2)
            .expect("update");
        assert_eq!(first.source().source_id(), second.source().source_id());
        assert_eq!(
            frozen
                .documents()
                .next()
                .expect("frozen document")
                .version(),
            1
        );
        assert_eq!(
            state
                .snapshot()
                .documents()
                .next()
                .expect("current document")
                .version(),
            2
        );
        assert!(matches!(
            state.update("a.ts", Arc::<str>::from("const c = 3;"), 2),
            Err(ServiceError::VersionOutOfOrder { .. })
        ));
        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn open_snapshot_precedes_disk_until_close() {
        let (root, mut state) = state();
        fs::write(root.join("a.ts"), "const disk = 1;").expect("write disk source");
        state
            .open("a.ts", Arc::<str>::from("const memory = 2;"), 1)
            .expect("open");
        let open = state
            .ensure_document(Path::new("a.ts"))
            .expect("open snapshot");
        assert!(open.source().source_text().as_str().contains("memory"));
        state.close("a.ts").expect("close");
        let closed = state
            .ensure_document(Path::new("a.ts"))
            .expect("disk snapshot");
        assert!(closed.source().source_text().as_str().contains("disk"));
        fs::remove_dir_all(root).expect("remove root");
    }
    #[test]
    fn pre_cancelled_open_does_not_mutate_state() {
        let (root, mut state) = state();
        let cancel = CancellationToken::new();
        cancel.cancel();

        assert!(matches!(
            state.open_with_cancel("a.ts", Arc::<str>::from("const value = 1;"), 1, &cancel,),
            Err(ServiceError::Cancelled)
        ));
        assert_eq!(state.snapshot().documents().count(), 0);
        assert_eq!(state.next_source_id, 0);
        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn cancelled_commit_new_document_leaves_state_untouched() {
        let (root, mut state) = state();
        let snapshot = state
            .open("a.ts", Arc::<str>::from("const value = 1;"), 1)
            .expect("open");
        let before = state.snapshot();
        let next_source_id = state.next_source_id;
        let cancel = CancellationToken::new();
        cancel.cancel();

        assert!(matches!(
            state.commit_new_document(root.join("b.ts"), snapshot, &cancel),
            Err(ServiceError::Cancelled)
        ));
        let after = state.snapshot();
        assert_eq!(before.documents.len(), after.documents.len());
        for (path, document) in &before.documents {
            assert!(Arc::ptr_eq(
                document,
                after.documents.get(path).expect("document remains present")
            ));
        }
        assert_eq!(state.next_source_id, next_source_id);
        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn cancelled_commit_replacement_document_leaves_state_untouched() {
        let (root, mut state) = state();
        let snapshot = state
            .open("a.ts", Arc::<str>::from("const value = 1;"), 1)
            .expect("open");
        let path = state
            .documents
            .keys()
            .next()
            .expect("open document")
            .clone();
        let before = state.snapshot();
        let next_source_id = state.next_source_id;
        let cancel = CancellationToken::new();
        cancel.cancel();

        assert!(matches!(
            state.commit_replacement_document(path, snapshot, &cancel),
            Err(ServiceError::Cancelled)
        ));
        let after = state.snapshot();
        assert_eq!(before.documents.len(), after.documents.len());
        for (path, document) in &before.documents {
            assert!(Arc::ptr_eq(
                document,
                after.documents.get(path).expect("document remains present")
            ));
        }
        assert_eq!(state.next_source_id, next_source_id);
        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn build_snapshot_honors_pre_cancelled_token() {
        let cancel = CancellationToken::new();
        cancel.cancel();

        assert!(matches!(
            build_snapshot_with_cancel(
                std::path::PathBuf::from("a.ts"),
                SourceId::new(0),
                Arc::<str>::from("const value = 1;"),
                1,
                true,
                None,
                &cancel,
            ),
            Err(ServiceError::Cancelled)
        ));
    }
}
