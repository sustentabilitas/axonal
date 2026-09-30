//! Uniquely named scratch paths, removed on drop unless persisted.

use std::{
    fs, io,
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
};

static SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) struct Temp {
    path: PathBuf,
    keep: bool,
}

impl Temp {
    /// Reserves `<dir>/<prefix><pid>-<seq><suffix>` without creating it; `dir` is created.
    pub(crate) fn reserve(dir: &Path, prefix: &str, suffix: &str) -> io::Result<Temp> {
        fs::create_dir_all(dir)?;
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        Ok(Temp {
            path: dir.join(format!("{prefix}{}-{seq}{suffix}", process::id())),
            keep: false,
        })
    }

    pub(crate) fn file(dir: &Path, prefix: &str, suffix: &str) -> io::Result<(Temp, fs::File)> {
        let temp = Temp::reserve(dir, prefix, suffix)?;
        let file = fs::File::create_new(&temp.path)?;
        Ok((temp, file))
    }

    pub(crate) fn dir(parent: &Path) -> io::Result<Temp> {
        let temp = Temp::reserve(parent, "", "")?;
        fs::create_dir(&temp.path)?;
        Ok(temp)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn persist(mut self, to: &Path) -> io::Result<()> {
        fs::rename(&self.path, to)?;
        self.keep = true;
        Ok(())
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        if !self.keep {
            let _ = match fs::symlink_metadata(&self.path) {
                Ok(meta) if meta.is_dir() => fs::remove_dir_all(&self.path),
                _ => fs::remove_file(&self.path),
            };
        }
    }
}
