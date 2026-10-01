//! Content-addressed task results: an output archive plus metadata, behind a `Store`.

pub mod archive;
pub mod local;
mod temp;

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::hash::Key;

pub use archive::Packed;
pub use local::Local;

/// Workspace-relative scratch space for archives being packed and restores being staged,
/// beside the cache so both can be renamed into place.
pub const TMP_DIR: &str = ".axonal/tmp";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub key: Key,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub logs: String,
    /// Workspace-relative output files in the archive; their count bounds extraction.
    pub outputs: Vec<PathBuf>,
    /// Total size of the output files, which bounds extraction.
    pub unpacked_bytes: u64,
    pub archive_blake3: String,
}

impl Meta {
    pub fn new(key: Key, exit_code: i32, duration_ms: u64, logs: String, packed: &Packed) -> Meta {
        Meta {
            key,
            exit_code,
            duration_ms,
            logs,
            outputs: packed.outputs.clone(),
            unpacked_bytes: packed.unpacked_bytes,
            archive_blake3: packed.blake3.clone(),
        }
    }
}

/// A verified hit: the archive matched `meta.archive_blake3` when it was opened, and is
/// positioned at its start.
#[derive(Debug)]
pub struct Entry {
    pub meta: Meta,
    pub archive: fs::File,
}

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("archive checksum mismatch: expected {expected}, got {actual}")]
    Corrupt { expected: String, actual: String },
    #[error("entry for key `{actual}` found under key `{expected}`")]
    WrongKey { expected: Key, actual: Key },
    #[error("malformed cache metadata: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("invalid cache key `{0}`")]
    InvalidKey(Key),
    #[error("output `{}` is a symlink, which can't be cached", .0.display())]
    Symlink(PathBuf),
    #[error("`{}` is not a regular file", .0.display())]
    NotAFile(PathBuf),
    #[error("`{}` is not a plain relative path", .0.display())]
    InvalidPath(PathBuf),
    #[error("archive exceeds its declared or the maximum size")]
    TooLarge,
    #[error("archive contents differ from the recorded outputs")]
    OutputsMismatch,
    #[error("`{}` is in the way of restored outputs", .0.display())]
    Blocked(PathBuf),
    #[error("unreadable archive: {0}")]
    BadArchive(io::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl CacheError {
    /// Whether a failed restore was caused by the archive's contents, so the entry
    /// should be removed. `InvalidPath` also covers stale paths, which `existing_files`
    /// never produces.
    pub fn is_archive_fault(&self) -> bool {
        matches!(
            self,
            CacheError::TooLarge
                | CacheError::OutputsMismatch
                | CacheError::InvalidPath(_)
                | CacheError::NotAFile(_)
                | CacheError::BadArchive(_)
        )
    }

    /// Whether the stored entry itself is bad, rather than the attempt to read it.
    pub fn is_corrupt(&self) -> bool {
        matches!(
            self,
            CacheError::Corrupt { .. } | CacheError::WrongKey { .. } | CacheError::Malformed(_)
        )
    }
}

/// Maps a missing file or directory to `None`.
fn existing<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Failures are never fatal: callers treat a failed read as a miss and a failed write
/// as a skipped save.
pub trait Store: Send + Sync {
    /// `Ok(None)` is a miss. Corrupt entries are errors, so they can be reported, and are
    /// removed.
    fn get(&self, key: &Key) -> Result<Option<Entry>, CacheError>;
    /// Moves the archive file at `archive`, described by `meta`, into the store. The file
    /// is consumed even if the put fails.
    fn put(&self, key: &Key, meta: &Meta, archive: &Path) -> Result<(), CacheError>;
    /// Deletes the entry, if any, e.g. after its restore failed with an archive fault.
    fn remove(&self, key: &Key) -> Result<(), CacheError>;
}
