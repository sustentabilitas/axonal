//! Content-addressed task results: an output archive plus metadata, behind a `Store`.

pub mod archive;
pub mod local;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::hash::Key;

pub use local::Local;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub exit_code: i32,
    pub duration_ms: u64,
    pub logs: String,
    /// Workspace-relative output files in the archive.
    pub outputs: Vec<PathBuf>,
    pub archive_blake3: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub meta: Meta,
    pub archive: Vec<u8>,
}

impl Entry {
    pub fn new(
        exit_code: i32,
        duration_ms: u64,
        logs: String,
        outputs: Vec<PathBuf>,
        archive: Vec<u8>,
    ) -> Entry {
        let archive_blake3 = blake3::hash(&archive).to_hex().to_string();
        Entry {
            meta: Meta {
                exit_code,
                duration_ms,
                logs,
                outputs,
                archive_blake3,
            },
            archive,
        }
    }

    pub fn verify(&self) -> anyhow::Result<()> {
        let actual = blake3::hash(&self.archive).to_hex();
        anyhow::ensure!(
            actual.as_str() == self.meta.archive_blake3,
            "archive checksum mismatch"
        );
        Ok(())
    }
}

/// Failures are never fatal: callers treat a failed read as a miss and a failed write
/// as a skipped save.
pub trait Store: Send + Sync {
    fn get(&self, key: &Key) -> anyhow::Result<Option<Entry>>;
    fn put(&self, key: &Key, entry: &Entry) -> anyhow::Result<()>;
}
