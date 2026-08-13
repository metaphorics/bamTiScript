//! Capability for a private native-artifact cache.
//!
//! The Windows implementation pins every validated path component by handle.
//! Other targets only hold a path that the caller has already validated.

use std::{
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{GuardedDir, GuardedFile, PrivateCacheRoot};

/// A cache root that has passed the platform trust checks.
#[cfg(not(windows))]
#[derive(Debug)]
pub struct PrivateCacheRoot {
    path: PathBuf,
}

#[cfg(not(windows))]
impl PrivateCacheRoot {
    /// Wraps a path after the caller has completed its existing platform checks.
    #[must_use]
    pub fn from_prevalidated(path: PathBuf) -> Self {
        Self { path }
    }

    /// Returns the validated cache-root path.
    #[must_use]
    pub fn root_path(&self) -> &Path {
        &self.path
    }
}

/// A cached archive whose integrity remains pinned while this value is alive.
#[derive(Debug)]
pub struct HeldArchive {
    path: PathBuf,
    #[cfg(windows)]
    _hold: GuardedFile,
}

impl HeldArchive {
    /// Wraps an archive after the caller has completed its existing platform checks.
    #[cfg(not(windows))]
    #[must_use]
    pub fn from_prevalidated(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(windows)]
    pub(super) fn held(hold: GuardedFile) -> Self {
        Self {
            path: hold.path().to_owned(),
            _hold: hold,
        }
    }

    /// Returns the archive path while retaining its integrity guard.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// A cache-path trust or integrity failure.
#[derive(Debug)]
pub enum CacheGuardError {
    /// An operating-system operation failed for the named path.
    Io { path: PathBuf, source: io::Error },
    /// The path did not name a directory.
    NotADirectory { path: PathBuf },
    /// A path component or artifact was a reparse point.
    ReparsePoint { path: PathBuf },
    /// The object had no effective access-control list.
    MissingDacl { path: PathBuf },
    /// The object owner can mutate the cache but is not trusted.
    UntrustedOwner { path: PathBuf, owner: String },
    /// An access-control entry grants mutation to an untrusted principal.
    UntrustedWriteAce { path: PathBuf, trustee: String },
    /// The access-control entry cannot be evaluated safely.
    UnsupportedAce { path: PathBuf, ace_type: u8 },
    /// A cached archive did not match the embedded runtime.
    ArchiveMismatch { path: PathBuf },
    /// Fresh-name generation exhausted its bounded retry set.
    NameAttemptsExhausted { parent: PathBuf },
}

impl CacheGuardError {
    /// Returns the path whose validation or operation failed.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Io { path, .. }
            | Self::NotADirectory { path }
            | Self::ReparsePoint { path }
            | Self::MissingDacl { path }
            | Self::UntrustedOwner { path, .. }
            | Self::UntrustedWriteAce { path, .. }
            | Self::UnsupportedAce { path, .. }
            | Self::ArchiveMismatch { path } => path,
            Self::NameAttemptsExhausted { parent } => parent,
        }
    }

    #[cfg(windows)]
    pub(super) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for CacheGuardError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "cache operation failed for `{}`: {source}",
                    path.display()
                )
            }
            Self::NotADirectory { path } => {
                write!(
                    formatter,
                    "cache path `{}` is not a directory",
                    path.display()
                )
            }
            Self::ReparsePoint { path } => {
                write!(
                    formatter,
                    "cache path `{}` is a reparse point",
                    path.display()
                )
            }
            Self::MissingDacl { path } => {
                write!(
                    formatter,
                    "cache path `{}` has no protective DACL",
                    path.display()
                )
            }
            Self::UntrustedOwner { path, owner } => write!(
                formatter,
                "cache path `{}` has untrusted owner `{owner}`",
                path.display()
            ),
            Self::UntrustedWriteAce { path, trustee } => write!(
                formatter,
                "cache path `{}` grants mutation to `{trustee}`",
                path.display()
            ),
            Self::UnsupportedAce { path, ace_type } => write!(
                formatter,
                "cache path `{}` has unsupported ACE type {ace_type}",
                path.display()
            ),
            Self::ArchiveMismatch { path } => write!(
                formatter,
                "cached archive `{}` does not match the embedded runtime",
                path.display()
            ),
            Self::NameAttemptsExhausted { parent } => write!(
                formatter,
                "could not allocate a fresh cache name under `{}`",
                parent.display()
            ),
        }
    }
}

impl Error for CacheGuardError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
