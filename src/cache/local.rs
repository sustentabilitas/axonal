//! The local cache in `.axonal/cache`: `<key>.tar.zst` plus `<key>.json`, evicted by
//! least-recent use (a hit refreshes the metadata file's mtime).
//!
//! Every operation holds a cache-wide lock on `.lock`, shared for reads and exclusive for
//! writes, eviction and cleaning, so neither threads nor concurrent axonal processes can
//! observe or evict a half-written entry.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

use tap::{Pipe, Tap};

use super::{Entry, Store};
use crate::hash::Key;

const LOCK: &str = ".lock";
const TMP: &str = ".tmp";
const ARCHIVE: &str = ".tar.zst";
const META: &str = ".json";

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct Local {
    dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub entries: usize,
    pub bytes: u64,
}

struct Item {
    key: Key,
    used: SystemTime,
    bytes: u64,
}

#[derive(Clone, Copy)]
enum Lock {
    Shared,
    Exclusive,
}

impl Local {
    pub fn new(workspace_root: &Path) -> Local {
        Local {
            dir: workspace_root.join(".axonal/cache"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn paths(&self, key: &Key) -> (PathBuf, PathBuf) {
        (
            self.dir.join(format!("{key}{ARCHIVE}")),
            self.dir.join(format!("{key}{META}")),
        )
    }

    fn checked_paths(&self, key: &Key) -> anyhow::Result<(PathBuf, PathBuf)> {
        anyhow::ensure!(is_key(&key.0), "invalid cache key `{key}`");
        Ok(self.paths(key))
    }

    /// `None` when the cache directory does not exist; the lock is released on drop.
    fn lock(&self, mode: Lock) -> io::Result<Option<fs::File>> {
        fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(LOCK))
            .pipe(existing)?
            .map(|file| {
                match mode {
                    Lock::Shared => file.lock_shared(),
                    Lock::Exclusive => file.lock(),
                }
                .map(|()| file)
            })
            .transpose()
    }

    fn names(&self) -> io::Result<Vec<String>> {
        existing(fs::read_dir(&self.dir))?
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .collect::<Vec<_>>()
            .pipe(Ok)
    }

    fn items(&self) -> io::Result<Vec<Item>> {
        self.names()?
            .iter()
            .filter_map(|name| name.strip_suffix(META).filter(|stem| is_key(stem)))
            .filter_map(|stem| {
                let key = Key(stem.to_string());
                let (archive, meta) = self.paths(&key);
                let meta = fs::metadata(meta).ok()?;
                Some(Item {
                    used: meta.modified().ok()?,
                    bytes: meta.len() + fs::metadata(archive).map_or(0, |m| m.len()),
                    key,
                })
            })
            .collect::<Vec<_>>()
            .pipe(Ok)
    }

    /// Metadata first, so a partly removed entry is never visible.
    fn remove(&self, key: &Key) -> io::Result<()> {
        let (archive, meta) = self.paths(key);
        existing(fs::remove_file(meta))?;
        existing(fs::remove_file(archive)).map(drop)
    }

    /// Deletes temp files and archives without metadata left by interrupted writes. Only
    /// safe under the exclusive lock, which every in-progress write holds.
    fn sweep(&self) -> io::Result<()> {
        self.names()?
            .into_iter()
            .filter(|name| {
                name.ends_with(TMP)
                    || name
                        .strip_suffix(ARCHIVE)
                        .is_some_and(|stem| !self.dir.join(format!("{stem}{META}")).exists())
            })
            .try_for_each(|name| existing(fs::remove_file(self.dir.join(name))).map(drop))
    }

    pub fn stats(&self) -> io::Result<Stats> {
        let _lock = self.lock(Lock::Shared)?;
        let items = self.items()?;
        Ok(Stats {
            entries: items.len(),
            bytes: items.iter().map(|i| i.bytes).sum(),
        })
    }

    /// Removes least-recently-used entries until the cache fits in `max_bytes`.
    pub fn evict(&self, max_bytes: u64) -> io::Result<usize> {
        let Some(_lock) = self.lock(Lock::Exclusive)? else {
            return Ok(0);
        };
        self.sweep()?;
        let items = self
            .items()?
            .tap_mut(|items| items.sort_by(|a, b| (a.used, &a.key).cmp(&(b.used, &b.key))));
        let total = items.iter().map(|i| i.bytes).sum::<u64>();
        items
            .iter()
            .scan(total, |total, item| {
                (*total > max_bytes).then(|| {
                    *total -= item.bytes;
                    item
                })
            })
            .try_fold(0, |removed, item| {
                self.remove(&item.key).map(|()| removed + 1)
            })
    }

    /// Removes every entry, keeping only the lock file so concurrent users stay serialized.
    pub fn clean(&self) -> io::Result<()> {
        let Some(_lock) = self.lock(Lock::Exclusive)? else {
            return Ok(());
        };
        self.names()?
            .into_iter()
            .filter(|name| name != LOCK)
            .try_for_each(|name| existing(fs::remove_file(self.dir.join(name))).map(drop))
    }
}

impl Store for Local {
    fn get(&self, key: &Key) -> anyhow::Result<Option<Entry>> {
        let (archive, meta) = self.checked_paths(key)?;
        let Some(_lock) = self.lock(Lock::Shared)? else {
            return Ok(None);
        };
        let Some(meta_bytes) = existing(fs::read(&meta))? else {
            return Ok(None);
        };
        let entry = Entry {
            meta: serde_json::from_slice(&meta_bytes)?,
            archive: fs::read(&archive)?,
        };
        entry.verify()?;
        let _ = fs::File::options()
            .write(true)
            .open(&meta)
            .and_then(|f| f.set_modified(SystemTime::now()));
        Ok(Some(entry))
    }

    /// The old metadata goes first and the new metadata last, so an entry is only visible
    /// once complete, even if the write is interrupted.
    fn put(&self, key: &Key, entry: &Entry) -> anyhow::Result<()> {
        let (archive, meta) = self.checked_paths(key)?;
        fs::create_dir_all(&self.dir)?;
        let _lock = self.lock(Lock::Exclusive)?;
        existing(fs::remove_file(&meta))?;
        write_atomic(&archive, &entry.archive)?;
        write_atomic(&meta, &serde_json::to_vec(&entry.meta)?)?;
        Ok(())
    }
}

/// Keys become file names, so they must not contain separators, dots or `..`.
fn is_key(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Maps a missing file or directory to `None`.
fn existing<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Writes a uniquely named temp file beside `path`, then renames it into place.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .expect("cache paths have file names")
        .to_string_lossy();
    let tmp = path.with_file_name(format!(
        ".{name}.{}.{}{TMP}",
        process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::File::options()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::archive;
    use std::time::Duration;

    fn entry(logs: &str) -> Entry {
        Entry::new(
            0,
            5,
            logs.into(),
            vec![],
            archive::pack(Path::new("."), &[]).unwrap(),
        )
    }

    /// Entries that differ in both metadata and archive bytes.
    fn distinct(tag: &str) -> Entry {
        Entry::new(
            0,
            5,
            tag.into(),
            vec![],
            format!("archive {tag}").into_bytes(),
        )
    }

    fn age(store: &Local, key: &Key, secs: u64) {
        let (_, meta) = store.paths(key);
        fs::File::options()
            .write(true)
            .open(meta)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    }

    fn names(store: &Local) -> Vec<String> {
        fs::read_dir(store.dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>()
            .tap_mut(|names| names.sort())
    }

    #[test]
    fn put_then_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        let key = Key("k1".into());
        store.put(&key, &entry("hello")).unwrap();
        assert_eq!(store.get(&key).unwrap(), Some(entry("hello")));
        assert_eq!(store.get(&Key("missing".into())).unwrap(), None);
    }

    #[test]
    fn corrupt_archives_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        let key = Key("k1".into());
        store.put(&key, &entry("hello")).unwrap();
        fs::write(store.paths(&key).0, b"garbage").unwrap();
        assert!(store.get(&key).is_err());
    }

    #[test]
    fn eviction_removes_least_recently_used_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        let keys = ["k1", "k2", "k3"].map(|k| Key(k.into()));
        for (i, key) in keys.iter().enumerate() {
            store.put(key, &entry("same size")).unwrap();
            age(&store, key, 300 - 100 * i as u64);
        }
        store.get(&keys[0]).unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(stats.entries, 3);
        let removed = store.evict(stats.bytes / 3 * 2).unwrap();
        assert_eq!(removed, 1);
        assert!(store.get(&keys[1]).unwrap().is_none());
        assert!(store.get(&keys[0]).unwrap().is_some());
        assert!(store.get(&keys[2]).unwrap().is_some());
    }

    #[test]
    fn clean_removes_everything() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        store.put(&Key("k1".into()), &entry("x")).unwrap();
        store.clean().unwrap();
        assert_eq!(store.stats().unwrap().entries, 0);
    }

    #[test]
    fn a_missing_cache_directory_is_empty_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        assert_eq!(store.get(&Key("k1".into())).unwrap(), None);
        assert_eq!(store.stats().unwrap().entries, 0);
        assert_eq!(store.evict(0).unwrap(), 0);
        store.clean().unwrap();
        assert!(!store.dir().exists());
    }

    #[test]
    fn keys_must_be_plain_file_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        for bad in ["", "../escape", "a/b", ".hidden", "k.json"] {
            let key = Key(bad.into());
            assert!(store.put(&key, &entry("x")).is_err(), "{bad:?}");
            assert!(store.get(&key).is_err(), "{bad:?}");
        }
        assert!(!dir.path().join(".axonal/escape.json").exists());
    }

    #[test]
    fn concurrent_puts_of_the_same_key_leave_a_consistent_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        let key = Key("k1".into());
        std::thread::scope(|s| {
            (0..8).for_each(|t| {
                let (store, key) = (&store, &key);
                s.spawn(move || {
                    (0..25).for_each(|i| {
                        store.put(key, &distinct(&format!("{t}/{i}"))).unwrap();
                        assert!(store.get(key).unwrap().is_some());
                    })
                });
            })
        });
        store.get(&key).unwrap().unwrap().verify().unwrap();
        assert_eq!(names(&store), [".lock", "k1.json", "k1.tar.zst"]);
    }

    #[test]
    fn eviction_never_breaks_an_entry_being_written() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        let key = Key("k1".into());
        std::thread::scope(|s| {
            s.spawn(|| (0..200).for_each(|i| store.put(&key, &distinct(&i.to_string())).unwrap()));
            s.spawn(|| {
                (0..200).for_each(|_| {
                    store.evict(0).unwrap();
                })
            });
            s.spawn(|| {
                (0..200).for_each(|_| {
                    store.get(&key).unwrap();
                })
            });
        });
        store.get(&key).unwrap();
    }

    #[test]
    fn eviction_sweeps_partial_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        store.put(&Key("k1".into()), &entry("x")).unwrap();
        fs::write(store.dir().join(".k2.json.1.0.tmp"), "partial").unwrap();
        fs::write(store.paths(&Key("k2".into())).0, "orphan").unwrap();
        assert_eq!(store.evict(u64::MAX).unwrap(), 0);
        assert_eq!(names(&store), [".lock", "k1.json", "k1.tar.zst"]);
    }
}
