//! The local cache in `.axonal/cache`: `<key>.tar.zst` plus `<key>.json`, evicted by
//! least-recent use (a hit refreshes the metadata file's mtime).
//!
//! Entry files are written to temp files first and only ever replaced by rename. Each
//! operation briefly holds a cache-wide lock: shared to read entries, exclusive to swap
//! them in, evict or clean. It is both a `flock` on `.lock`, for other processes, and an
//! in-process `RwLock`, for threads where `flock` is emulated per process (NFS). An
//! archive opened under the lock can be verified and restored after it is released.

use std::{
    collections::HashMap,
    fs,
    io::{self, Seek, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard},
    time::{Duration, SystemTime},
};

use tap::Tap;

use super::{CacheError, Entry, Meta, Store, TMP_DIR, existing, temp::Temp};
use crate::hash::Key;

const LOCK: &str = ".lock";
const TMP: &str = ".tmp";
const ARCHIVE: &str = ".tar.zst";
const META: &str = ".json";
/// Temp files older than this belong to writes that died.
const STALE_TMP: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone)]
pub struct Local {
    dir: PathBuf,
    scratch: PathBuf,
    threads: Arc<RwLock<()>>,
    #[cfg(test)]
    pause: Option<Arc<std::sync::Barrier>>,
}

/// Held for the duration of a locked section; fields release in order on drop.
struct Guard<'a> {
    _flock: Option<fs::File>,
    _read: Option<RwLockReadGuard<'a, ()>>,
    _write: Option<RwLockWriteGuard<'a, ()>>,
}

/// One in-process lock per cache directory, shared by every `Local` for it however its
/// path is spelled.
fn thread_lock(dir: &Path) -> Arc<RwLock<()>> {
    static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<RwLock<()>>>>> =
        LazyLock::new(Default::default);
    LOCKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(canonical(dir))
        .or_default()
        .clone()
}

/// `path` with its deepest existing ancestor canonicalized, so it is the same before and
/// after the rest is created. The rest doesn't exist, so it holds no symlinks and its
/// `..`s are resolved lexically.
fn canonical(path: &Path) -> PathBuf {
    path.ancestors()
        .find_map(|a| Some((a, a.canonicalize().ok()?)))
        .and_then(|(a, c)| {
            path.strip_prefix(a).ok().map(|rest| {
                rest.components().fold(c, |acc, part| match part {
                    Component::ParentDir => acc.tap_mut(|p| {
                        p.pop();
                    }),
                    Component::CurDir => acc,
                    part => acc.tap_mut(|p| p.push(part)),
                })
            })
        })
        .unwrap_or_else(|| path.to_path_buf())
}

/// The process that made a scratch entry (`<pid>-<seq>…`) or temp file
/// (`.<name>.<pid>-<seq>.tmp`).
fn owner(name: &str) -> Option<u32> {
    let tail = name
        .strip_suffix(TMP)
        .map_or(name, |stem| stem.rsplit('.').next().unwrap_or(stem));
    tail.split('-').next()?.parse().ok()
}

#[cfg(unix)]
fn is_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 sends nothing; it only checks that the process exists.
    let found = unsafe { libc::kill(pid, 0) } == 0;
    found || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn is_alive(_: u32) -> bool {
    false
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
        let dir = workspace_root.join(".axonal/cache");
        Local {
            threads: thread_lock(&dir),
            scratch: workspace_root.join(TMP_DIR),
            dir,
            #[cfg(test)]
            pause: None,
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

    /// `None` when the cache directory does not exist. Shared locks open `.lock` read-only
    /// and, in a read-only cache without one, rely on the in-process lock alone.
    fn lock(&self, mode: Lock) -> io::Result<Option<Guard<'_>>> {
        let (read, write) = match mode {
            Lock::Shared => (
                Some(self.threads.read().unwrap_or_else(PoisonError::into_inner)),
                None,
            ),
            Lock::Exclusive => (
                None,
                Some(self.threads.write().unwrap_or_else(PoisonError::into_inner)),
            ),
        };
        let path = self.dir.join(LOCK);
        let opened = match mode {
            Lock::Shared => existing(fs::File::open(&path))?,
            Lock::Exclusive => None,
        };
        let flock = match opened.map_or_else(|| create_lock(&path), Ok) {
            Ok(file) => Some(file),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) if matches!(mode, Lock::Shared) && is_read_only(&e) => None,
            Err(e) => return Err(e),
        };
        if let Some(file) = &flock {
            match mode {
                Lock::Shared => file.lock_shared()?,
                Lock::Exclusive => file.lock()?,
            }
        }
        Ok(Some(Guard {
            _flock: flock,
            _read: read,
            _write: write,
        }))
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
    fn remove_files(&self, key: &Key) -> io::Result<()> {
        let (archive, meta) = self.paths(key);
        existing(fs::remove_file(meta))?;
        existing(fs::remove_file(archive)).map(drop)
    }

    /// Under the exclusive lock, deletes what interrupted work left behind: temp files and
    /// scratch entries older than [`STALE_TMP`] whose process is gone (others may still be
    /// in use), and archives without metadata (entries are only swapped in under this
    /// lock).
    fn sweep(&self) -> io::Result<()> {
        let cutoff = SystemTime::now() - STALE_TMP;
        let abandoned = |dir: &Path, name: &str| {
            !owner(name).is_some_and(is_alive)
                && fs::symlink_metadata(dir.join(name))
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t < cutoff)
        };
        let orphan = |name: &str| {
            name.strip_suffix(ARCHIVE)
                .is_some_and(|stem| !self.dir.join(format!("{stem}{META}")).exists())
        };
        self.names()?
            .into_iter()
            .filter(|name| (name.ends_with(TMP) && abandoned(&self.dir, name)) || orphan(name))
            .try_for_each(|name| remove_path(&self.dir.join(name)))?;
        existing(fs::read_dir(&self.scratch))?
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| abandoned(&self.scratch, name))
            .try_for_each(|name| remove_path(&self.scratch.join(name)))
    }

    /// Removes an entry found bad, unless a put has replaced its metadata since.
    fn discard(&self, key: &Key, meta_bytes: &[u8]) -> io::Result<()> {
        let Some(_lock) = self.lock(Lock::Exclusive)? else {
            return Ok(());
        };
        if existing(fs::read(self.paths(key).1))?.is_some_and(|current| current == meta_bytes) {
            self.remove_files(key)?;
        }
        Ok(())
    }

    /// Moves `archive` beside the entries under a temp name, copying it across
    /// filesystems, and marks it fresh so a concurrent sweep leaves it alone.
    fn stage_archive(&self, key: &Key, archive: &Path) -> io::Result<Temp> {
        let temp = Temp::reserve(&self.dir, &format!(".{key}{ARCHIVE}."), TMP)?;
        match fs::rename(archive, temp.path()) {
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                fs::copy(archive, temp.path())?;
                fs::remove_file(archive)?;
            }
            result => result?,
        }
        fs::File::open(temp.path())?.set_modified(SystemTime::now())?;
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
                self.remove_files(&item.key).map(|()| removed + 1)
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
            .try_for_each(|name| remove_path(&self.dir.join(name)))
    }
}

impl Store for Local {
    fn remove(&self, key: &Key) -> Result<(), CacheError> {
        self.checked_paths(key)?;
        let Some(_lock) = self.lock(Lock::Exclusive)? else {
            return Ok(());
        };
        Ok(self.remove_files(key)?)
    }

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

    /// Both files are staged unlocked. Under the lock the old metadata goes first and the
    /// new metadata last, so an entry is only visible once complete, even if the write is
    /// interrupted.
    fn put(&self, key: &Key, meta: &Meta, archive: &Path) -> Result<(), CacheError> {
        let (archive_path, meta_path) = self.checked_paths(key)?;
        if meta.key != *key {
            return Err(CacheError::WrongKey {
                expected: key.clone(),
                actual: meta.key.clone(),
            });
        }
        fs::create_dir_all(&self.dir)?;
        let staged_archive = self.stage_archive(key, archive)?;
        let staged_meta = self.stage_meta(key, meta)?;
        #[cfg(test)]
        if let Some(barrier) = &self.pause {
            barrier.wait();
            barrier.wait();
        }
        let _lock = self.lock(Lock::Exclusive)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "the cache directory disappeared")
        })?;
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

fn create_lock(path: &Path) -> io::Result<fs::File> {
    fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn is_read_only(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
    )
}

/// Removes a file, link or whole directory without following links.
fn remove_path(path: &Path) -> io::Result<()> {
    let removed = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        _ => fs::remove_file(path),
    };
    existing(removed).map(drop)
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
        let packed = archive::pack(src.path(), &[PathBuf::from("out.txt")], 0).unwrap();
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
        fs::File::open(path)
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
    fn remove_deletes_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        let (k1, k2) = (Key("k1".into()), Key("k2".into()));
        put(&store, &k1, "hello");
        put(&store, &k2, "other");
        store.remove(&k1).unwrap();
        assert!(store.get(&k1).unwrap().is_none());
        assert!(store.get(&k2).unwrap().is_some());
        store.remove(&k1).unwrap();
        assert!(matches!(
            store.remove(&Key("../x".into())),
            Err(CacheError::InvalidKey(_))
        ));
        assert_eq!(names(&store), [".lock", "k2.json", "k2.tar.zst"]);
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

    #[test]
    fn eviction_sweeps_only_temp_files_older_than_an_hour() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        put(&store, &Key("k1".into()), "x");
        let scratch = dir.path().join(TMP_DIR);
        fs::create_dir_all(scratch.join("2147483647-0/new")).unwrap();
        for path in [
            store.dir().join(".k2.json.2147483647-0.tmp"),
            store.dir().join(".k3.json.2147483647-1.tmp"),
            scratch.join("2147483647-1.tar.zst"),
            scratch.join("2147483647-2.tar.zst"),
        ] {
            fs::write(path, "partial").unwrap();
        }
        age(&store.dir().join(".k3.json.2147483647-1.tmp"), 2 * 3600);
        age(&scratch.join("2147483647-2.tar.zst"), 2 * 3600);
        age(&scratch.join("2147483647-0"), 2 * 3600);
        store.evict(u64::MAX).unwrap();
        assert_eq!(
            names(&store),
            [
                ".k2.json.2147483647-0.tmp",
                ".lock",
                "k1.json",
                "k1.tar.zst"
            ]
        );
        let left = fs::read_dir(&scratch)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(left, ["2147483647-1.tar.zst"]);
    }

    #[cfg(unix)]
    #[test]
    fn eviction_spares_old_scratch_entries_of_live_processes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        put(&store, &Key("k1".into()), "x");
        let live = format!("{}-99", std::process::id());
        let parked = dir.path().join(TMP_DIR).join(&live).join("old/dist/a.js");
        fs::create_dir_all(parked.parent().unwrap()).unwrap();
        fs::write(&parked, "parked").unwrap();
        let temp = store.dir().join(format!(".k2.json.{live}.tmp"));
        fs::write(&temp, "partial").unwrap();
        age(&dir.path().join(TMP_DIR).join(&live), 2 * 3600);
        age(&temp, 2 * 3600);
        store.evict(u64::MAX).unwrap();
        assert!(parked.exists());
        assert!(temp.exists());
    }

    #[test]
    fn differently_spelled_roots_share_the_in_process_lock() {
        let dir = tempfile::tempdir().unwrap();
        let spellings = [
            dir.path().to_path_buf(),
            dir.path().join("."),
            dir.path().canonicalize().unwrap(),
            dir.path().join("missing/./.."),
        ];
        let before = spellings.each_ref().map(|root| Local::new(root));
        assert!(
            before
                .iter()
                .all(|l| Arc::ptr_eq(&l.threads, &before[0].threads))
        );
        put(&before[1], &Key("k1".into()), "x");
        let after = spellings.each_ref().map(|root| Local::new(root));
        assert!(
            after
                .iter()
                .all(|l| Arc::ptr_eq(&l.threads, &before[0].threads))
        );
    }

    #[test]
    fn a_slow_put_does_not_block_gets_of_other_keys() {
        use std::sync::{Arc, Barrier, mpsc};
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        let (k1, k2) = (Key("k1".into()), Key("k2".into()));
        put(&store, &k1, "ready");

        let barrier = Arc::new(Barrier::new(2));
        let slow = Local {
            pause: Some(barrier.clone()),
            ..store.clone()
        };
        let (src, meta, packed) = staged(&k2, "slow");
        let writer = std::thread::spawn(move || {
            let _src = src;
            slow.put(&k2, &meta, packed.path()).map(drop)
        });
        barrier.wait();
        let (tx, rx) = mpsc::channel();
        let reader = store.clone();
        std::thread::spawn(move || tx.send(reader.get(&k1).map(|e| e.is_some())).unwrap());
        let got = rx.recv_timeout(Duration::from_secs(10));
        barrier.wait();
        assert!(matches!(got, Ok(Ok(true))), "{got:?}");
        writer.join().unwrap().unwrap();
        assert!(store.get(&Key("k2".into())).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn read_only_caches_still_serve_gets() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        let key = Key("k1".into());
        put(&store, &key, "hello");
        let mode = |path: &Path, mode| {
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap()
        };
        mode(&store.dir().join(LOCK), 0o444);
        mode(store.dir(), 0o555);
        let got = store.get(&key);
        mode(store.dir(), 0o755);
        assert_eq!(restored(got.unwrap().unwrap()), "hello");
    }

    #[test]
    fn clean_removes_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        put(&store, &Key("k1".into()), "x");
        fs::create_dir_all(store.dir().join("sub/deeper")).unwrap();
        fs::write(store.dir().join("sub/deeper/x"), "x").unwrap();
        store.clean().unwrap();
        assert_eq!(names(&store), [".lock"]);
    }

    #[test]
    fn put_fails_when_the_lock_cannot_be_taken() {
        let dir = tempfile::tempdir().unwrap();
        let store = Local::new(dir.path());
        fs::create_dir_all(store.dir().join(LOCK)).unwrap();
        let key = Key("k1".into());
        let (_src, meta, packed) = staged(&key, "x");
        assert!(store.put(&key, &meta, packed.path()).is_err());
        assert_eq!(names(&store), [".lock"]);
    }
}
