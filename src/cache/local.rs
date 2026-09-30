//! The local cache in `.axonal/cache`: `<key>.tar.zst` plus `<key>.json`, evicted by
//! least-recent use (a hit refreshes the metadata file's mtime).
//!
//! Operations hold a cache-wide lock on `.lock`, shared for reads and exclusive for
//! writes, eviction and cleaning. Entry files are only ever replaced by rename, so an
//! archive opened under the lock can be verified and restored after it is released.

use std::{
    fs,
    io::{self, Seek, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

use tap::Tap;

use super::{CacheError, Entry, Meta, Store, existing, temp::Temp};
use crate::hash::Key;

const LOCK: &str = ".lock";
const TMP: &str = ".tmp";
const ARCHIVE: &str = ".tar.zst";
const META: &str = ".json";

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

    fn checked_paths(&self, key: &Key) -> Result<(PathBuf, PathBuf), CacheError> {
        if is_key(&key.0) {
            Ok(self.paths(key))
        } else {
            Err(CacheError::InvalidKey(key.clone()))
        }
    }

    /// `None` when the cache directory does not exist; the lock is released on drop.
    fn lock(&self, mode: Lock) -> io::Result<Option<fs::File>> {
        existing(
            fs::File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(self.dir.join(LOCK)),
        )?
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
        Ok(existing(fs::read_dir(&self.dir))?
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .collect())
    }

    fn items(&self) -> io::Result<Vec<Item>> {
        Ok(self
            .names()?
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
            .collect())
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

    /// Removes an entry found bad, unless a put has replaced its metadata since.
    fn discard(&self, key: &Key, meta_bytes: &[u8]) -> io::Result<()> {
        let Some(_lock) = self.lock(Lock::Exclusive)? else {
            return Ok(());
        };
        if existing(fs::read(self.paths(key).1))?.is_some_and(|current| current == meta_bytes) {
            self.remove(key)?;
        }
        Ok(())
    }

    /// Moves `archive` beside the entries under a temp name, copying it across
    /// filesystems.
    fn stage_archive(&self, key: &Key, archive: &Path) -> io::Result<Temp> {
        let temp = Temp::reserve(&self.dir, &format!(".{key}{ARCHIVE}."), TMP)?;
        match fs::rename(archive, temp.path()) {
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                fs::copy(archive, temp.path())?;
                fs::remove_file(archive)?;
            }
            result => result?,
        }
        Ok(temp)
    }

    fn stage_meta(&self, key: &Key, meta: &Meta) -> Result<Temp, CacheError> {
        let (temp, mut file) = Temp::file(&self.dir, &format!(".{key}{META}."), TMP)?;
        file.write_all(&serde_json::to_vec(meta)?)?;
        Ok(temp)
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
    fn get(&self, key: &Key) -> Result<Option<Entry>, CacheError> {
        let (archive_path, meta_path) = self.checked_paths(key)?;
        let (meta_bytes, archive) = {
            let Some(_lock) = self.lock(Lock::Shared)? else {
                return Ok(None);
            };
            let Some(meta_bytes) = existing(fs::read(&meta_path))? else {
                return Ok(None);
            };
            (meta_bytes, existing(fs::File::open(&archive_path))?)
        };
        let Some(mut archive) = archive else {
            let _ = self.discard(key, &meta_bytes);
            return Ok(None);
        };
        match verify(key, &meta_bytes, &mut archive) {
            Ok(meta) => {
                archive.rewind()?;
                let _ = fs::File::options()
                    .write(true)
                    .open(&meta_path)
                    .and_then(|f| f.set_modified(SystemTime::now()));
                Ok(Some(Entry { meta, archive }))
            }
            Err(e) => {
                if e.is_corrupt() {
                    let _ = self.discard(key, &meta_bytes);
                }
                Err(e)
            }
        }
    }

    /// The old metadata goes first and the new metadata last, so an entry is only visible
    /// once complete, even if the write is interrupted.
    fn put(&self, key: &Key, meta: &Meta, archive: &Path) -> Result<(), CacheError> {
        let (archive_path, meta_path) = self.checked_paths(key)?;
        if meta.key != *key {
            return Err(CacheError::WrongKey {
                expected: key.clone(),
                actual: meta.key.clone(),
            });
        }
        fs::create_dir_all(&self.dir)?;
        let _lock = self.lock(Lock::Exclusive)?;
        let staged_archive = self.stage_archive(key, archive)?;
        let staged_meta = self.stage_meta(key, meta)?;
        existing(fs::remove_file(&meta_path))?;
        staged_archive.persist(&archive_path)?;
        staged_meta.persist(&meta_path)?;
        Ok(())
    }
}

fn verify(key: &Key, meta_bytes: &[u8], archive: &mut fs::File) -> Result<Meta, CacheError> {
    let meta: Meta = serde_json::from_slice(meta_bytes)?;
    if meta.key != *key {
        return Err(CacheError::WrongKey {
            expected: key.clone(),
            actual: meta.key,
        });
    }
    let actual = blake3::Hasher::new()
        .update_reader(archive)?
        .finalize()
        .to_hex()
        .to_string();
    if actual != meta.archive_blake3 {
        return Err(CacheError::Corrupt {
            expected: meta.archive_blake3,
            actual,
        });
    }
    Ok(meta)
}

/// Keys become file names, so they must not contain separators, dots or `..`.
fn is_key(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{Packed, archive};
    use std::time::Duration;

    /// A packed one-file archive whose contents and logs are `tag`, from its own source tree.
    fn staged(key: &Key, tag: &str) -> (tempfile::TempDir, Meta, Packed) {
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("out.txt"), tag).unwrap();
        let packed = archive::pack(src.path(), &[PathBuf::from("out.txt")]).unwrap();
        let meta = Meta::new(key.clone(), 0, 5, tag.into(), &packed);
        (src, meta, packed)
    }

    fn put(store: &Local, key: &Key, tag: &str) -> Meta {
        let (_src, meta, packed) = staged(key, tag);
        store.put(key, &meta, packed.path()).unwrap();
        meta
    }

    fn restored(entry: Entry) -> String {
        let dst = tempfile::tempdir().unwrap();
        archive::restore(dst.path(), &entry.meta, &entry.archive, &[]).unwrap();
        fs::read_to_string(dst.path().join("out.txt")).unwrap()
    }

    fn age(path: &Path, secs: u64) {
        fs::File::options()
            .write(true)
            .open(path)
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
        let meta = put(&store, &key, "hello");
        let entry = store.get(&key).unwrap().unwrap();
        assert_eq!(entry.meta, meta);
        assert_eq!(restored(entry), "hello");
        assert!(store.get(&Key("missing".into())).unwrap().is_none());
    }

    #[test]
    fn put_moves_the_archive_into_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        let key = Key("k1".into());
        let (_src, meta, packed) = staged(&key, "hello");
        store.put(&key, &meta, packed.path()).unwrap();
        assert!(!packed.path().exists());
        assert_eq!(names(&store), [".lock", "k1.json", "k1.tar.zst"]);
    }

    #[test]
    fn put_rejects_metadata_for_another_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        let (_src, meta, packed) = staged(&Key("k1".into()), "hello");
        let err = store.put(&Key("k2".into()), &meta, packed.path());
        assert!(matches!(err, Err(CacheError::WrongKey { .. })), "{err:?}");
    }

    #[test]
    fn corrupt_archives_are_errors_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        let key = Key("k1".into());
        put(&store, &key, "hello");
        fs::write(store.paths(&key).0, b"garbage").unwrap();
        let err = store.get(&key);
        assert!(matches!(err, Err(CacheError::Corrupt { .. })), "{err:?}");
        assert!(store.get(&key).unwrap().is_none());
        assert_eq!(names(&store), [".lock"]);
    }

    #[test]
    fn malformed_metadata_is_an_error_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        let key = Key("k1".into());
        put(&store, &key, "hello");
        fs::write(store.paths(&key).1, b"{").unwrap();
        let err = store.get(&key);
        assert!(matches!(err, Err(CacheError::Malformed(_))), "{err:?}");
        assert!(store.get(&key).unwrap().is_none());
    }

    #[test]
    fn entries_copied_to_another_key_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        let (k1, k2) = (Key("k1".into()), Key("k2".into()));
        put(&store, &k1, "hello");
        let ((a1, m1), (a2, m2)) = (store.paths(&k1), store.paths(&k2));
        fs::copy(a1, a2).unwrap();
        fs::copy(m1, m2).unwrap();
        let err = store.get(&k2);
        assert!(matches!(err, Err(CacheError::WrongKey { .. })), "{err:?}");
        assert!(store.get(&k2).unwrap().is_none());
        assert!(store.get(&k1).unwrap().is_some());
    }

    #[test]
    fn metadata_without_an_archive_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        let key = Key("k1".into());
        put(&store, &key, "hello");
        fs::remove_file(store.paths(&key).0).unwrap();
        assert!(store.get(&key).unwrap().is_none());
        assert_eq!(names(&store), [".lock"]);
    }

    #[test]
    fn eviction_removes_least_recently_used_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        let keys = ["k1", "k2", "k3"].map(|k| Key(k.into()));
        for (i, key) in keys.iter().enumerate() {
            put(&store, key, "same size");
            age(&store.paths(key).1, 300 - 100 * i as u64);
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
        put(&store, &Key("k1".into()), "x");
        store.clean().unwrap();
        assert_eq!(store.stats().unwrap().entries, 0);
    }

    #[test]
    fn a_missing_cache_directory_is_empty_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        assert!(store.get(&Key("k1".into())).unwrap().is_none());
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
            let (_src, meta, packed) = staged(&key, "x");
            let err = store.put(&key, &meta, packed.path());
            assert!(matches!(err, Err(CacheError::InvalidKey(_))), "{bad:?}");
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
                        put(store, key, &format!("{t}/{i}"));
                        assert!(store.get(key).unwrap().is_some());
                    })
                });
            })
        });
        let entry = store.get(&key).unwrap().unwrap();
        assert_eq!(restored(entry), store.get(&key).unwrap().unwrap().meta.logs);
        assert_eq!(names(&store), [".lock", "k1.json", "k1.tar.zst"]);
    }

    #[test]
    fn eviction_never_breaks_an_entry_being_written() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        let key = Key("k1".into());
        std::thread::scope(|s| {
            s.spawn(|| (0..100).for_each(|i| drop(put(&store, &key, &i.to_string()))));
            s.spawn(|| {
                (0..100).for_each(|_| {
                    store.evict(0).unwrap();
                })
            });
            s.spawn(|| {
                (0..100).for_each(|_| {
                    store.get(&key).unwrap();
                })
            });
        });
        store.get(&key).unwrap();
    }

    #[test]
    fn eviction_sweeps_archives_without_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        put(&store, &Key("k1".into()), "x");
        fs::write(store.paths(&Key("k2".into())).0, "orphan").unwrap();
        assert_eq!(store.evict(u64::MAX).unwrap(), 0);
        assert_eq!(names(&store), [".lock", "k1.json", "k1.tar.zst"]);
    }
}
