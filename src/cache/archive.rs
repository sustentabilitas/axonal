//! Output archives: tar streams of regular files, compressed with zstd, packed and
//! restored without holding them in memory.

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, BufWriter, Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime},
};

use super::{CacheError, Meta, TMP_DIR, existing, temp::Temp};

const LEVEL: i32 = 3;
/// Largest decoder window accepted (128 MiB); level 3 needs far less.
const WINDOW_LOG_MAX: u32 = 27;
/// Caps on any archive, whatever its metadata declares.
pub const MAX_UNPACKED_BYTES: u64 = 16 << 30;
pub const MAX_FILES: usize = 1 << 20;

/// A packed archive in the workspace's scratch directory, deleted on drop unless a store
/// has moved it away.
#[derive(Debug)]
pub struct Packed {
    file: Temp,
    pub outputs: Vec<PathBuf>,
    pub unpacked_bytes: u64,
    pub blake3: String,
}

impl Packed {
    pub fn path(&self) -> &Path {
        self.file.path()
    }
}

/// Archives `files`, workspace-relative regular files, under their relative paths. A
/// symlink anywhere on an output's path is `CacheError::Symlink`: such outputs are not
/// cacheable. Each file is archived at its length when opened; one that shrinks while
/// being read fails the pack.
///
/// `workers` zstd threads compress in parallel, each buffering about 20 MB; 0 compresses
/// on the calling thread.
pub fn pack(root: &Path, files: &[PathBuf], workers: u32) -> Result<Packed, CacheError> {
    pack_with(root, files, workers, &|_| {})
}

/// `opened` runs once each file's length is fixed, before it is read.
fn pack_with(
    root: &Path,
    files: &[PathBuf],
    workers: u32,
    opened: &dyn Fn(&Path),
) -> Result<Packed, CacheError> {
    if files.len() > MAX_FILES {
        return Err(CacheError::TooLarge);
    }
    let (temp, file) = Temp::file(&root.join(TMP_DIR), "", ".tar.zst")?;
    let mut encoder = zstd::Encoder::new(Hashing::new(BufWriter::new(file)), LEVEL)?;
    if workers > 0 {
        encoder.multithread(workers)?;
    }
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    let unpacked_bytes = files.iter().try_fold(0u64, |total, rel| {
        let (mut file, meta) = open_output(root, rel)?;
        let len = meta.len();
        opened(rel);
        let mut header = tar::Header::new_gnu();
        header.set_metadata(&meta);
        header.set_size(len);
        let exact = Exact {
            file: (&mut file).take(len),
            path: rel,
        };
        builder.append_data(&mut header, rel, exact)?;
        total
            .checked_add(len)
            .filter(|&total| total <= MAX_UNPACKED_BYTES)
            .ok_or(CacheError::TooLarge)
    })?;
    let (writer, blake3) = builder.into_inner()?.finish()?.finish();
    writer
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?;
    Ok(Packed {
        file: temp,
        outputs: files.to_vec(),
        unpacked_bytes,
        blake3,
    })
}

/// Opens a regular file with its metadata, refusing symlinks on its path and swaps after
/// the check.
fn open_output(root: &Path, rel: &Path) -> Result<(File, fs::Metadata), CacheError> {
    if !is_plain(rel) {
        return Err(CacheError::InvalidPath(rel.to_path_buf()));
    }
    let leaf = prefixes(rel).try_fold(None, |_, prefix| {
        let meta = fs::symlink_metadata(root.join(&prefix))?;
        if meta.is_symlink() {
            return Err(CacheError::Symlink(prefix));
        }
        Ok(Some(meta))
    })?;
    let leaf = leaf.filter(fs::Metadata::is_file);
    let file = File::open(root.join(rel))?;
    let meta = file.metadata()?;
    match leaf {
        Some(leaf) if same_file(&leaf, &meta) => Ok((file, meta)),
        Some(_) => Err(CacheError::Symlink(rel.to_path_buf())),
        None => Err(CacheError::NotAFile(rel.to_path_buf())),
    }
}

/// The first `len` bytes of a file, failing if it ends sooner.
struct Exact<'a> {
    file: io::Take<&'a mut File>,
    path: &'a Path,
}

impl Read for Exact<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(buf)?;
        if n == 0 && !buf.is_empty() && self.file.limit() > 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("`{}` shrank while being packed", self.path.display()),
            ));
        }
        Ok(n)
    }
}

#[cfg(unix)]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    (a.dev(), a.ino()) == (b.dev(), b.ino())
}

#[cfg(not(unix))]
fn same_file(_: &fs::Metadata, b: &fs::Metadata) -> bool {
    b.is_file()
}

/// A writer that hashes everything written through it.
struct Hashing<W> {
    inner: W,
    hasher: blake3::Hasher,
}

impl<W> Hashing<W> {
    fn new(inner: W) -> Self {
        Hashing {
            inner,
            hasher: blake3::Hasher::new(),
        }
    }

    fn finish(self) -> (W, String) {
        (self.inner, self.hasher.finalize().to_hex().to_string())
    }
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Replaces `stale`, the workspace-relative outputs currently present, with the archive's
/// files.
///
/// The archive is extracted into a staging directory under [`TMP_DIR`] and must hold
/// exactly `meta.outputs`, without duplicates and within `meta.unpacked_bytes`. Stale
/// files (links themselves, never their targets) are then moved aside and the staged
/// files renamed into place. Nothing is read or written through a symlinked directory:
/// one in the way is `CacheError::Blocked`, unless it is itself stale. On success,
/// directories left empty by removing stale files are removed too, up to `root`.
///
/// The restore is all or nothing for every error it returns: those moves are undone and
/// the workspace is as it was. It is not if the process is killed midway, which can leave
/// some outputs placed and some stale files parked in the staging directory; the next run
/// then sees whatever outputs are present as stale, and `Local::evict` sweeps the staging
/// directory once its process has gone.
///
/// Errors for which [`CacheError::is_archive_fault`] holds mean the entry itself is
/// unusable, so the caller should remove it.
pub fn restore(
    root: &Path,
    meta: &Meta,
    archive: impl Read,
    stale: &[PathBuf],
) -> Result<(), CacheError> {
    let staging = Temp::dir(&root.join(TMP_DIR))?;
    let (new, old) = (staging.path().join("new"), staging.path().join("old"));
    extract(meta, archive, &new)?;
    let mut commit = Commit {
        root,
        new: &new,
        old: &old,
        done: Vec::new(),
    };
    commit
        .run(&meta.outputs, stale)
        .inspect_err(|_| commit.undo())?;
    prune(root, stale);
    Ok(())
}

/// Best effort: removes each stale file's ancestors, deepest first, while they are
/// empty, never `root` itself.
fn prune(root: &Path, stale: &[PathBuf]) {
    stale.iter().for_each(|rel| {
        let _ = rel
            .ancestors()
            .skip(1)
            .take_while(|dir| !dir.as_os_str().is_empty())
            .try_for_each(|dir| fs::remove_dir(root.join(dir)));
    });
}

fn extract(meta: &Meta, archive: impl Read, dst: &Path) -> Result<(), CacheError> {
    let expected = meta.outputs.iter().cloned().collect::<BTreeSet<_>>();
    if expected.len() != meta.outputs.len() {
        return Err(CacheError::OutputsMismatch);
    }
    let max_files = meta.outputs.len().min(MAX_FILES);
    let max_bytes = meta.unpacked_bytes.min(MAX_UNPACKED_BYTES);
    let mut decoder = zstd::Decoder::new(archive)?;
    decoder.window_log_max(WINDOW_LOG_MAX)?;
    let mut archive = tar::Archive::new(decoder);
    let (seen, bytes) = archive
        .entries()
        .map_err(CacheError::BadArchive)?
        .try_fold((BTreeSet::new(), 0u64), |(mut seen, bytes), entry| {
            let mut entry = entry.map_err(CacheError::BadArchive)?;
            let rel = entry_path(&entry)?;
            let bytes = bytes.saturating_add(entry.size());
            if seen.len() >= max_files || bytes > max_bytes {
                return Err(CacheError::TooLarge);
            }
            if !seen.insert(rel.clone()) {
                return Err(CacheError::OutputsMismatch);
            }
            write_file(&mut entry, &dst.join(rel))?;
            Ok((seen, bytes))
        })?;
    if seen != expected || bytes != meta.unpacked_bytes {
        return Err(CacheError::OutputsMismatch);
    }
    Ok(())
}

/// Only regular files at plain relative paths; parent directories are implied.
fn entry_path<R: Read>(entry: &tar::Entry<R>) -> Result<PathBuf, CacheError> {
    let path = entry.path().map_err(CacheError::BadArchive)?.into_owned();
    if entry.header().entry_type() != tar::EntryType::Regular {
        Err(CacheError::NotAFile(path))
    } else if !is_plain(&path) {
        Err(CacheError::InvalidPath(path))
    } else {
        Ok(path)
    }
}

/// `path` is inside the fresh staging directory, which holds nothing but directories and
/// regular files.
fn write_file<R: Read>(entry: &mut tar::Entry<R>, path: &Path) -> Result<(), CacheError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = File::create_new(path)?;
    let mut buf = vec![0; 64 << 10];
    loop {
        match entry.read(&mut buf).map_err(CacheError::BadArchive)? {
            0 => break,
            n => file.write_all(&buf[..n])?,
        }
    }
    let header = entry.header();
    if let Ok(secs) = header.mtime() {
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))?;
    }
    #[cfg(unix)]
    if let Ok(mode) = header.mode() {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
    }
    Ok(())
}

/// Non-empty, with only normal components.
fn is_plain(path: &Path) -> bool {
    path.components().next().is_some()
        && path.components().all(|c| matches!(c, Component::Normal(_)))
}

/// `a`, `a/b`, `a/b/c` for `a/b/c`.
fn prefixes(rel: &Path) -> impl Iterator<Item = PathBuf> {
    rel.components().scan(PathBuf::new(), |acc, c| {
        acc.push(c);
        Some(acc.clone())
    })
}

/// Workspace changes made so far by a restore, in order, so they can be undone.
enum Step {
    MovedAside(PathBuf),
    Created(PathBuf),
    Placed(PathBuf),
}

struct Commit<'a> {
    root: &'a Path,
    new: &'a Path,
    old: &'a Path,
    done: Vec<Step>,
}

impl Commit<'_> {
    fn run(&mut self, outputs: &[PathBuf], stale: &[PathBuf]) -> Result<(), CacheError> {
        stale.iter().try_for_each(|rel| self.move_aside(rel))?;
        outputs.iter().try_for_each(|rel| self.place(rel))
    }

    /// Moves whatever is at `rel`, without following it, into the staging directory.
    fn move_aside(&mut self, rel: &Path) -> Result<(), CacheError> {
        if !is_plain(rel) {
            return Err(CacheError::InvalidPath(rel.to_path_buf()));
        }
        if !self.parents(rel, false)? {
            return Ok(());
        }
        match existing(fs::symlink_metadata(self.root.join(rel)))? {
            None => Ok(()),
            Some(meta) if meta.is_dir() => Err(CacheError::Blocked(rel.to_path_buf())),
            Some(_) => {
                let aside = self.old.join(rel);
                if existing(fs::symlink_metadata(&aside))?.is_some() {
                    return Err(CacheError::OutputsMismatch);
                }
                if let Some(parent) = aside.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(self.root.join(rel), aside)?;
                self.done.push(Step::MovedAside(rel.to_path_buf()));
                Ok(())
            }
        }
    }

    fn place(&mut self, rel: &Path) -> Result<(), CacheError> {
        self.parents(rel, true)?;
        self.move_aside(rel)?;
        fs::rename(self.new.join(rel), self.root.join(rel))?;
        self.done.push(Step::Placed(rel.to_path_buf()));
        Ok(())
    }

    /// Whether every proper ancestor of `rel` is a real directory. Missing ones are
    /// created when `create`, and otherwise make this `false`; anything else is in the way.
    fn parents(&mut self, rel: &Path, create: bool) -> Result<bool, CacheError> {
        rel.parent()
            .into_iter()
            .flat_map(prefixes)
            .try_fold(true, |exists, dir| {
                if !exists {
                    return Ok(false);
                }
                match existing(fs::symlink_metadata(self.root.join(&dir)))? {
                    Some(meta) if meta.is_dir() => Ok(true),
                    Some(_) => Err(CacheError::Blocked(dir)),
                    None if create => {
                        fs::create_dir(self.root.join(&dir))?;
                        self.done.push(Step::Created(dir));
                        Ok(true)
                    }
                    None => Ok(false),
                }
            })
    }

    /// Best effort: reverses every recorded step, newest first.
    fn undo(&mut self) {
        self.done.drain(..).rev().for_each(|step| {
            let _ = match step {
                Step::Placed(rel) => fs::remove_file(self.root.join(rel)),
                Step::Created(dir) => fs::remove_dir(self.root.join(dir)),
                Step::MovedAside(rel) => fs::rename(self.old.join(&rel), self.root.join(&rel)),
            };
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Key;

    fn touch(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn paths(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    fn meta(outputs: &[&str], unpacked_bytes: u64) -> Meta {
        Meta {
            key: Key("k".into()),
            exit_code: 0,
            duration_ms: 1,
            logs: vec![],
            outputs: paths(outputs),
            unpacked_bytes,
            archive_blake3: String::new(),
        }
    }

    /// Packs `files` (path, contents) from a scratch source tree.
    fn packed(files: &[(&str, &str)]) -> (tempfile::TempDir, Packed, Meta) {
        let src = tempfile::tempdir().unwrap();
        files
            .iter()
            .for_each(|(rel, body)| touch(src.path(), rel, body));
        let rels = files
            .iter()
            .map(|(rel, _)| PathBuf::from(rel))
            .collect::<Vec<_>>();
        let packed = pack(src.path(), &rels, 0).unwrap();
        let meta = Meta::new(Key("k".into()), 0, 1, vec![], &packed);
        (src, packed, meta)
    }

    fn read(path: &Path) -> Vec<u8> {
        fs::read(path).unwrap()
    }

    /// Every file and symlink under `root` with its contents (link text for links),
    /// excluding `.axonal`.
    fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        crate::files::list(root)
            .unwrap()
            .into_iter()
            .map(|rel| {
                let path = root.join(&rel);
                let body = match fs::read_link(&path) {
                    Ok(target) => target.into_os_string().into_encoded_bytes(),
                    Err(_) => read(&path),
                };
                (rel, body)
            })
            .collect()
    }

    fn scratch_is_empty(root: &Path) -> bool {
        fs::read_dir(root.join(TMP_DIR)).map_or(true, |mut d| d.next().is_none())
    }

    /// A single-entry archive written header-first, bypassing the builder's checks.
    fn raw(path: &str, kind: tar::EntryType, mode: u32, link: Option<&str>) -> Vec<u8> {
        let data: &[u8] = if kind == tar::EntryType::Regular {
            b"evil"
        } else {
            b""
        };
        let mut header = tar::Header::new_gnu();
        let gnu = header.as_gnu_mut().unwrap();
        gnu.name[..path.len()].copy_from_slice(path.as_bytes());
        if let Some(link) = link {
            gnu.linkname[..link.len()].copy_from_slice(link.as_bytes());
        }
        header.set_entry_type(kind);
        header.set_mode(mode);
        header.set_size(data.len() as u64);
        header.set_cksum();
        let mut builder = tar::Builder::new(zstd::Encoder::new(Vec::new(), 3).unwrap());
        builder.append(&header, data).unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    /// `(outer, workspace)`: a workspace directory with room to escape into.
    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let outer = tempfile::tempdir().unwrap();
        let ws = outer.path().join("ws");
        fs::create_dir(&ws).unwrap();
        (outer, ws)
    }

    #[test]
    fn round_trips_files_under_their_relative_paths() {
        let (_src, packed, meta) = packed(&[("libs/a/dist/out.txt", "built")]);
        assert_eq!(meta.outputs, paths(&["libs/a/dist/out.txt"]));
        assert_eq!(meta.unpacked_bytes, 5);
        assert_eq!(
            meta.archive_blake3,
            blake3::hash(&read(packed.path())).to_hex().as_str()
        );

        let dst = tempfile::tempdir().unwrap();
        restore(dst.path(), &meta, File::open(packed.path()).unwrap(), &[]).unwrap();
        assert_eq!(
            fs::read_to_string(dst.path().join("libs/a/dist/out.txt")).unwrap(),
            "built"
        );
        assert!(scratch_is_empty(dst.path()));
    }

    #[test]
    fn empty_archives_are_valid() {
        let (_src, packed, meta) = packed(&[]);
        let dst = tempfile::tempdir().unwrap();
        restore(dst.path(), &meta, File::open(packed.path()).unwrap(), &[]).unwrap();
    }

    #[test]
    fn dropping_a_packed_archive_removes_it() {
        let (_src, packed, _) = packed(&[("a.txt", "a")]);
        let path = packed.path().to_path_buf();
        assert!(path.exists());
        drop(packed);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn modes_and_mtimes_are_restored() {
        use std::os::unix::fs::PermissionsExt;
        let src = tempfile::tempdir().unwrap();
        touch(src.path(), "bin/run", "#!/bin/sh");
        let path = src.path().join("bin/run");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o751)).unwrap();
        let mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let packed = pack(src.path(), &paths(&["bin/run"]), 0).unwrap();
        let meta = Meta::new(Key("k".into()), 0, 1, vec![], &packed);

        let dst = tempfile::tempdir().unwrap();
        restore(dst.path(), &meta, File::open(packed.path()).unwrap(), &[]).unwrap();
        let restored = fs::metadata(dst.path().join("bin/run")).unwrap();
        assert_eq!(restored.permissions().mode() & 0o777, 0o751);
        assert_eq!(restored.modified().unwrap(), mtime);
    }

    #[test]
    fn stale_outputs_are_removed_on_restore() {
        let (_src, packed, meta) = packed(&[("dist/out.js", "new")]);
        let dst = tempfile::tempdir().unwrap();
        touch(dst.path(), "dist/out.js", "old");
        touch(dst.path(), "dist/stale.js", "stale");
        let stale = paths(&["dist/out.js", "dist/stale.js"]);
        restore(
            dst.path(),
            &meta,
            File::open(packed.path()).unwrap(),
            &stale,
        )
        .unwrap();
        assert_eq!(
            snapshot(dst.path()),
            [(PathBuf::from("dist/out.js"), b"new".to_vec())]
        );
        assert!(scratch_is_empty(dst.path()));
    }

    #[test]
    fn a_restore_failing_while_extracting_leaves_the_workspace_unchanged() {
        let (_src, packed, meta) =
            packed(&[("dist/a.js", &"a".repeat(100_000)), ("dist/b.js", "b")]);
        let bytes = read(packed.path());
        let dst = tempfile::tempdir().unwrap();
        touch(dst.path(), "dist/a.js", "old");
        touch(dst.path(), "dist/stale.js", "stale");
        let before = snapshot(dst.path());
        let stale = paths(&["dist/a.js", "dist/stale.js"]);
        let truncated = &bytes[..bytes.len() / 2];
        assert!(restore(dst.path(), &meta, truncated, &stale).is_err());
        assert_eq!(snapshot(dst.path()), before);
        assert!(scratch_is_empty(dst.path()));
    }

    #[test]
    fn a_restore_failing_while_moving_outputs_into_place_leaves_the_workspace_unchanged() {
        let (_src, packed, meta) = packed(&[("dist/a.js", "new a"), ("dist/b.js", "new b")]);
        let dst = tempfile::tempdir().unwrap();
        touch(dst.path(), "dist/a.js", "old a");
        touch(dst.path(), "dist/stale.js", "stale");
        touch(
            dst.path(),
            "dist/b.js/in-the-way",
            "a directory where b.js goes",
        );
        let before = snapshot(dst.path());
        let stale = paths(&["dist/a.js", "dist/stale.js"]);
        let err = restore(
            dst.path(),
            &meta,
            File::open(packed.path()).unwrap(),
            &stale,
        );
        assert!(matches!(err, Err(CacheError::Blocked(_))), "{err:?}");
        assert_eq!(snapshot(dst.path()), before);
        assert!(scratch_is_empty(dst.path()));
    }

    #[test]
    fn archives_must_hold_exactly_the_recorded_outputs() {
        let (_src, packed, mut meta) = packed(&[("dist/a.js", "a")]);
        meta.outputs = paths(&["dist/other.js"]);
        let dst = tempfile::tempdir().unwrap();
        let err = restore(dst.path(), &meta, File::open(packed.path()).unwrap(), &[]);
        assert!(matches!(err, Err(CacheError::OutputsMismatch)), "{err:?}");
        assert!(snapshot(dst.path()).is_empty());
    }

    #[test]
    fn content_beyond_the_declared_size_or_count_is_rejected() {
        let (_src, packed, meta) = packed(&[("a.txt", &"x".repeat(4096)), ("b.txt", "b")]);
        let dst = tempfile::tempdir().unwrap();
        for declared in [
            Meta {
                unpacked_bytes: 10,
                ..meta.clone()
            },
            Meta {
                outputs: paths(&["a.txt"]),
                ..meta.clone()
            },
        ] {
            let err = restore(
                dst.path(),
                &declared,
                File::open(packed.path()).unwrap(),
                &[],
            );
            assert!(matches!(err, Err(CacheError::TooLarge)), "{err:?}");
        }
        assert!(snapshot(dst.path()).is_empty());
    }

    #[test]
    fn directory_entries_are_rejected() {
        let (_outer, ws) = workspace();
        let bytes = raw("dist", tar::EntryType::Directory, 0o000, None);
        let err = restore(&ws, &meta(&["dist"], 0), &bytes[..], &[]);
        assert!(matches!(err, Err(CacheError::NotAFile(_))), "{err:?}");
        assert!(!ws.join("dist").exists());
    }

    #[test]
    fn absolute_paths_are_rejected() {
        let (outer, ws) = workspace();
        let target = outer.path().join("abs.txt");
        let target = target.to_str().unwrap();
        let bytes = raw(target, tar::EntryType::Regular, 0o644, None);
        let err = restore(&ws, &meta(&[target], 4), &bytes[..], &[]);
        assert!(matches!(err, Err(CacheError::InvalidPath(_))), "{err:?}");
        assert!(!Path::new(target).exists());
        assert!(snapshot(&ws).is_empty());
    }

    #[test]
    fn parent_components_are_rejected() {
        let (outer, ws) = workspace();
        for path in [
            "../escape.txt",
            "a/../../escape.txt",
            "a/../b.txt",
            "./a.txt",
        ] {
            let bytes = raw(path, tar::EntryType::Regular, 0o644, None);
            let err = restore(&ws, &meta(&[path], 4), &bytes[..], &[]);
            assert!(
                matches!(err, Err(CacheError::InvalidPath(_))),
                "{path}: {err:?}"
            );
        }
        assert!(!outer.path().join("escape.txt").exists());
        assert!(snapshot(&ws).is_empty());
    }

    #[test]
    fn links_are_rejected() {
        let (outer, ws) = workspace();
        fs::write(outer.path().join("outside"), "secret").unwrap();
        let absolute = outer.path().to_str().unwrap().to_string();
        for (kind, target) in [
            (tar::EntryType::Symlink, absolute.as_str()),
            (tar::EntryType::Symlink, "../outside"),
            (tar::EntryType::Symlink, "inside"),
            (tar::EntryType::Link, "../outside"),
        ] {
            let bytes = raw("link", kind, 0o644, Some(target));
            let err = restore(&ws, &meta(&["link"], 0), &bytes[..], &[]);
            assert!(
                matches!(err, Err(CacheError::NotAFile(_))),
                "{target}: {err:?}"
            );
        }
        assert!(snapshot(&ws).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn workspace_symlinks_are_replaced_only_when_stale_and_never_followed() {
        let (_src, packed, meta) = packed(&[("dist/out.txt", "built")]);
        let (outer, ws) = workspace();
        fs::create_dir(outer.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(outer.path().join("elsewhere"), ws.join("dist")).unwrap();

        let err = restore(&ws, &meta, File::open(packed.path()).unwrap(), &[]);
        assert!(matches!(err, Err(CacheError::Blocked(_))), "{err:?}");
        restore(
            &ws,
            &meta,
            File::open(packed.path()).unwrap(),
            &paths(&["dist"]),
        )
        .unwrap();
        assert!(!fs::symlink_metadata(ws.join("dist")).unwrap().is_symlink());
        assert_eq!(
            fs::read_to_string(ws.join("dist/out.txt")).unwrap(),
            "built"
        );
        assert!(
            fs::read_dir(outer.path().join("elsewhere"))
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_glob_base_reported_by_existing_files_is_replaced() {
        let (_src, packed, meta) = packed(&[("libs/a/dist/out.txt", "built")]);
        let (outer, ws) = workspace();
        touch(outer.path(), "elsewhere/keep.txt", "keep");
        touch(&ws, "libs/a/package.json", "{}");
        std::os::unix::fs::symlink(outer.path().join("elsewhere"), ws.join("libs/a/dist")).unwrap();
        let stale = crate::files::Patterns::new(&["dist/**".into()])
            .unwrap()
            .existing_files(&ws, Path::new("libs/a"))
            .unwrap();
        assert_eq!(stale, paths(&["libs/a/dist"]));

        restore(&ws, &meta, File::open(packed.path()).unwrap(), &stale).unwrap();
        assert!(
            !fs::symlink_metadata(ws.join("libs/a/dist"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(ws.join("libs/a/dist/out.txt")).unwrap(),
            "built"
        );
        assert!(ws.join("libs/a/package.json").is_file());
        assert_eq!(
            fs::read_to_string(outer.path().join("elsewhere/keep.txt")).unwrap(),
            "keep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stale_files_behind_symlinked_directories_are_left_alone() {
        let (_src, packed, meta) = packed(&[("out.txt", "built")]);
        let (outer, ws) = workspace();
        touch(outer.path(), "elsewhere/keep.txt", "keep");
        std::os::unix::fs::symlink(outer.path().join("elsewhere"), ws.join("dist")).unwrap();
        let stale = paths(&["dist/keep.txt"]);
        let err = restore(&ws, &meta, File::open(packed.path()).unwrap(), &stale);
        assert!(matches!(err, Err(CacheError::Blocked(_))), "{err:?}");
        assert!(outer.path().join("elsewhere/keep.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_outputs_are_not_packed() {
        use std::os::unix::fs::symlink;
        let (outer, ws) = workspace();
        touch(outer.path(), "home/creds", "secret");
        touch(&ws, "dist/a.js", "a");
        symlink(outer.path().join("home/creds"), ws.join("dist/config")).unwrap();
        symlink("a.js", ws.join("dist/b.js")).unwrap();
        for leaf in ["dist/config", "dist/b.js"] {
            let err = pack(&ws, &paths(&["dist/a.js", leaf]), 0);
            assert!(
                matches!(&err, Err(CacheError::Symlink(p)) if p == Path::new(leaf)),
                "{leaf}: {err:?}"
            );
        }
        assert!(scratch_is_empty(&ws));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_not_followed_when_packing() {
        let (outer, ws) = workspace();
        touch(outer.path(), "elsewhere/x.js", "x");
        std::os::unix::fs::symlink(outer.path().join("elsewhere"), ws.join("dist")).unwrap();
        let err = pack(&ws, &paths(&["dist/x.js"]), 0);
        assert!(
            matches!(&err, Err(CacheError::Symlink(p)) if p == Path::new("dist")),
            "{err:?}"
        );
    }

    #[test]
    fn outputs_must_be_plain_relative_files() {
        let (outer, ws) = workspace();
        touch(outer.path(), "x", "x");
        fs::create_dir(ws.join("dir")).unwrap();
        let abs = outer.path().join("x");
        for bad in [Path::new("../x"), abs.as_path(), Path::new("./x")] {
            let err = pack(&ws, &[bad.to_path_buf()], 0);
            assert!(
                matches!(err, Err(CacheError::InvalidPath(_))),
                "{bad:?}: {err:?}"
            );
        }
        let err = pack(&ws, &paths(&["dir"]), 0);
        assert!(matches!(err, Err(CacheError::NotAFile(_))), "{err:?}");
    }

    #[test]
    fn duplicate_outputs_are_rejected_without_losing_originals() {
        let (_src, packed, mut meta) = packed(&[("dist/a.js", "NEW")]);
        meta.outputs = paths(&["dist/a.js", "dist/a.js"]);
        for stale in [vec![], paths(&["dist/a.js"])] {
            let dst = tempfile::tempdir().unwrap();
            touch(dst.path(), "dist/a.js", "ORIGINAL");
            let err = restore(
                dst.path(),
                &meta,
                File::open(packed.path()).unwrap(),
                &stale,
            );
            assert!(matches!(err, Err(CacheError::OutputsMismatch)), "{err:?}");
            assert_eq!(
                fs::read_to_string(dst.path().join("dist/a.js")).unwrap(),
                "ORIGINAL"
            );
            assert!(scratch_is_empty(dst.path()));
        }
    }

    #[test]
    fn moving_aside_never_overwrites_a_parked_file() {
        let ws = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let (new, old) = (staging.path().join("new"), staging.path().join("old"));
        touch(ws.path(), "a.js", "current");
        touch(&old, "a.js", "parked");
        let mut commit = Commit {
            root: ws.path(),
            new: &new,
            old: &old,
            done: Vec::new(),
        };
        assert!(commit.move_aside(Path::new("a.js")).is_err());
        assert_eq!(
            fs::read_to_string(ws.path().join("a.js")).unwrap(),
            "current"
        );
        assert_eq!(fs::read_to_string(old.join("a.js")).unwrap(), "parked");
    }

    #[test]
    fn a_file_growing_while_packed_is_archived_at_its_opened_length() {
        use std::io::Write;
        let src = tempfile::tempdir().unwrap();
        touch(src.path(), "log.txt", "first");
        let grow = |rel: &Path| {
            File::options()
                .append(true)
                .open(src.path().join(rel))
                .unwrap()
                .write_all(b" and more")
                .unwrap()
        };
        let packed = pack_with(src.path(), &paths(&["log.txt"]), 0, &grow).unwrap();
        assert_eq!(packed.unpacked_bytes, 5);
        let meta = Meta::new(Key("k".into()), 0, 1, vec![], &packed);
        let dst = tempfile::tempdir().unwrap();
        restore(dst.path(), &meta, File::open(packed.path()).unwrap(), &[]).unwrap();
        assert_eq!(
            fs::read_to_string(dst.path().join("log.txt")).unwrap(),
            "first"
        );
    }

    #[test]
    fn a_file_shrinking_while_packed_fails_the_pack() {
        let src = tempfile::tempdir().unwrap();
        touch(src.path(), "log.txt", "first");
        let shrink = |rel: &Path| {
            File::options()
                .write(true)
                .open(src.path().join(rel))
                .unwrap()
                .set_len(2)
                .unwrap()
        };
        assert!(pack_with(src.path(), &paths(&["log.txt"]), 0, &shrink).is_err());
        assert!(scratch_is_empty(src.path()));
    }

    #[test]
    fn multithreaded_packing_round_trips() {
        let src = tempfile::tempdir().unwrap();
        let body = "multithreaded ".repeat(200_000);
        touch(src.path(), "dist/big.txt", &body);
        let packed = pack(src.path(), &paths(&["dist/big.txt"]), 2).unwrap();
        let meta = Meta::new(Key("k".into()), 0, 1, vec![], &packed);
        let dst = tempfile::tempdir().unwrap();
        restore(dst.path(), &meta, File::open(packed.path()).unwrap(), &[]).unwrap();
        assert_eq!(
            fs::read_to_string(dst.path().join("dist/big.txt")).unwrap(),
            body
        );
    }

    #[test]
    fn archive_faults_are_told_apart_from_workspace_problems() {
        let (_src, packed, meta) = packed(&[("dist/a.js", &"a".repeat(100_000))]);
        let bytes = read(packed.path());
        let restore_into = |meta: &Meta, bytes: &[u8]| {
            let dst = tempfile::tempdir().unwrap();
            touch(dst.path(), "dist/a.js/in-the-way", "x");
            restore(dst.path(), meta, bytes, &[]).unwrap_err()
        };

        let truncated = restore_into(&meta, &bytes[..bytes.len() / 2]);
        assert!(
            matches!(truncated, CacheError::BadArchive(_)),
            "{truncated:?}"
        );
        let garbage = restore_into(&meta, b"not an archive");
        assert!(matches!(garbage, CacheError::BadArchive(_)), "{garbage:?}");
        let too_large = restore_into(
            &Meta {
                unpacked_bytes: 1,
                ..meta.clone()
            },
            &bytes,
        );
        let mismatch = restore_into(
            &Meta {
                outputs: paths(&["dist/b.js"]),
                ..meta.clone()
            },
            &bytes,
        );
        let link = raw("link", tar::EntryType::Symlink, 0o644, Some("x"));
        let not_a_file = restore_into(&meta, &link);
        let absolute = raw("/abs", tar::EntryType::Regular, 0o644, None);
        let invalid_path = restore_into(&meta, &absolute);
        for fault in [
            truncated,
            garbage,
            too_large,
            mismatch,
            not_a_file,
            invalid_path,
        ] {
            assert!(fault.is_archive_fault(), "{fault:?}");
        }

        let blocked = restore_into(&meta, &bytes);
        assert!(matches!(blocked, CacheError::Blocked(_)), "{blocked:?}");
        assert!(!blocked.is_archive_fault());
        assert!(!CacheError::Io(io::Error::other("disk full")).is_archive_fault());
    }

    #[test]
    fn directories_emptied_by_a_restore_are_removed() {
        let (_src, packed, meta) = packed(&[("dist/a.js", "new")]);
        let dst = tempfile::tempdir().unwrap();
        touch(dst.path(), "dist/old/deeper/x.js", "stale");
        touch(dst.path(), "dist/keep/y.js", "not an output");
        touch(dst.path(), "top.js", "stale");
        let stale = paths(&["dist/old/deeper/x.js", "top.js"]);
        restore(
            dst.path(),
            &meta,
            File::open(packed.path()).unwrap(),
            &stale,
        )
        .unwrap();
        assert!(!dst.path().join("dist/old").exists());
        assert!(dst.path().join("dist/keep/y.js").exists());
        assert!(dst.path().join("dist/a.js").exists());
        assert!(dst.path().exists());
    }

    /// Run with `cargo test --release -- --ignored packs_large_outputs` under
    /// `/usr/bin/time -l` to see peak memory stay far below the archive size.
    #[test]
    #[ignore]
    fn packs_large_outputs_in_bounded_memory() {
        use std::io::Write;
        let src = tempfile::tempdir().unwrap();
        let files = (0..200)
            .map(|i| {
                let rel = PathBuf::from(format!("dist/{i:03}.bin"));
                touch(src.path(), rel.to_str().unwrap(), "");
                let mut file = File::create(src.path().join(&rel)).unwrap();
                let mut state = i as u64 + 1;
                let block = (0..1 << 20)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        (state % 16) as u8
                    })
                    .collect::<Vec<u8>>();
                file.write_all(&block).unwrap();
                rel
            })
            .collect::<Vec<_>>();
        let packed = pack(src.path(), &files, 0).unwrap();
        assert_eq!(packed.unpacked_bytes, 200 << 20);
        let meta = Meta::new(Key("k".into()), 0, 1, vec![], &packed);
        let dst = tempfile::tempdir().unwrap();
        restore(dst.path(), &meta, File::open(packed.path()).unwrap(), &[]).unwrap();
        assert_eq!(crate::files::list(dst.path()).unwrap().len(), 200);
    }
}
