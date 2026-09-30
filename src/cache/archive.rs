//! Output archives: tar streams compressed with zstd.

use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

/// `files` are relative to `root` and keep those paths inside the archive. Symlinks are
/// stored as the files they point to.
pub fn pack(root: &Path, files: &[PathBuf]) -> io::Result<Vec<u8>> {
    files
        .iter()
        .try_fold(
            tar::Builder::new(zstd::Encoder::new(Vec::new(), 3)?),
            |mut builder, file| {
                builder
                    .append_path_with_name(root.join(file), file)
                    .map(|()| builder)
            },
        )?
        .into_inner()?
        .finish()
}

/// Extracts under `root`, creating it if missing. Only regular files and directories at
/// relative paths without `..` are accepted, and never through a symlink that leaves
/// `root`; anything else fails the restore, leaving earlier entries extracted.
pub fn unpack(root: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(root)?;
    let root = root.canonicalize()?;
    let mut archive = tar::Archive::new(zstd::Decoder::new(bytes)?);
    archive.entries()?.try_for_each(|entry| {
        let mut entry = entry?;
        check(&entry)?;
        entry.unpack_in(&root).map(drop)
    })
}

fn check<R: io::Read>(entry: &tar::Entry<R>) -> io::Result<()> {
    let path = entry.path()?;
    let kind = entry.header().entry_type();
    let invalid = |why: &str| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("archive entry `{}` {why}", path.display()),
        )
    };
    if !(kind.is_file() || kind.is_dir()) {
        return Err(invalid(&format!("has unsupported type {kind:?}")));
    }
    if !path
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(invalid("is not a plain relative path"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A single-entry archive written header-first, bypassing the builder's path checks.
    fn raw(path: &str, kind: tar::EntryType, link: Option<&str>) -> Vec<u8> {
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
        header.set_mode(0o644);
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

    fn is_empty(dir: &Path) -> bool {
        fs::read_dir(dir).unwrap().next().is_none()
    }

    #[test]
    fn round_trips_files_under_their_relative_paths() {
        let src = tempfile::tempdir().unwrap();
        fs::create_dir_all(src.path().join("libs/a/dist")).unwrap();
        fs::write(src.path().join("libs/a/dist/out.txt"), "built").unwrap();
        let bytes = pack(src.path(), &[PathBuf::from("libs/a/dist/out.txt")]).unwrap();

        let dst = tempfile::tempdir().unwrap();
        unpack(dst.path(), &bytes).unwrap();
        assert_eq!(
            fs::read_to_string(dst.path().join("libs/a/dist/out.txt")).unwrap(),
            "built"
        );
    }

    #[test]
    fn empty_archives_are_valid() {
        let dst = tempfile::tempdir().unwrap();
        unpack(dst.path(), &pack(dst.path(), &[]).unwrap()).unwrap();
    }

    #[test]
    fn absolute_paths_are_rejected() {
        let (outer, ws) = workspace();
        let target = outer.path().join("abs.txt");
        let bytes = raw(target.to_str().unwrap(), tar::EntryType::Regular, None);
        assert!(unpack(&ws, &bytes).is_err());
        assert!(!target.exists());
        assert!(is_empty(&ws));
    }

    #[test]
    fn parent_components_are_rejected() {
        let (outer, ws) = workspace();
        for path in ["../escape.txt", "a/../../escape.txt", "a/../b.txt"] {
            let bytes = raw(path, tar::EntryType::Regular, None);
            assert!(unpack(&ws, &bytes).is_err(), "{path}");
        }
        assert!(!outer.path().join("escape.txt").exists());
        assert!(is_empty(&ws));
    }

    #[test]
    fn symlinks_are_rejected() {
        let (outer, ws) = workspace();
        let absolute = outer.path().to_str().unwrap().to_string();
        for target in [absolute.as_str(), "..", "../outside", "a/../../outside"] {
            let bytes = raw("link", tar::EntryType::Symlink, Some(target));
            assert!(unpack(&ws, &bytes).is_err(), "{target}");
        }
        assert!(is_empty(&ws));
    }

    #[cfg(unix)]
    #[test]
    fn existing_symlinks_out_of_the_workspace_are_not_followed() {
        let (outer, ws) = workspace();
        let src = tempfile::tempdir().unwrap();
        fs::create_dir(src.path().join("dist")).unwrap();
        fs::write(src.path().join("dist/out.txt"), "built").unwrap();
        let bytes = pack(src.path(), &[PathBuf::from("dist/out.txt")]).unwrap();

        fs::create_dir(outer.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(outer.path().join("elsewhere"), ws.join("dist")).unwrap();
        assert!(unpack(&ws, &bytes).is_err());
        assert!(is_empty(&outer.path().join("elsewhere")));
    }

    #[test]
    fn hard_links_are_rejected() {
        let (outer, ws) = workspace();
        fs::write(outer.path().join("outside"), "secret").unwrap();
        let bytes = raw("hard", tar::EntryType::Link, Some("../outside"));
        assert!(unpack(&ws, &bytes).is_err());
        assert!(is_empty(&ws));
    }
}
