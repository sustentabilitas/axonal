//! Thin wrappers over the `git` CLI. Paths are relative to the directory commands run in
//! (the workspace root), which may be a subdirectory of the repository.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use crate::error::{Error, Result};

fn output(dir: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| Error::Git(format!("cannot run git: {e}")))
}

fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = output(dir, args)?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(Error::Git(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim().to_string()
}

pub fn toplevel(dir: &Path) -> Result<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"]).map(|b| PathBuf::from(text(b)))
}

pub fn rev_exists(dir: &Path, rev: &str) -> bool {
    git(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ],
    )
    .is_ok()
}

pub fn merge_base(dir: &Path, a: &str, b: &str) -> Result<String> {
    git(dir, &["merge-base", a, b]).map(text)
}

/// `merge-base(<branch>, head)`, falling back to `origin/<branch>`.
pub fn default_base(dir: &Path, branch: &str, head: &str) -> Result<String> {
    [branch.to_string(), format!("origin/{branch}")]
        .iter()
        .find(|b| rev_exists(dir, b))
        .ok_or_else(|| {
            Error::Git(format!(
                "cannot find `{branch}` or `origin/{branch}`; pass --base"
            ))
        })
        .and_then(|b| merge_base(dir, b, head))
}

/// Files changed between `base` and `head`; with no `head`, against the working tree,
/// including untracked files. Deleted files are included.
pub fn changed_files(dir: &Path, base: &str, head: Option<&str>) -> Result<BTreeSet<PathBuf>> {
    let mut args = vec![
        "diff",
        "--name-only",
        "--no-renames",
        "--relative",
        "-z",
        base,
    ];
    args.extend(head);
    let mut files = split_nul(&git(dir, &args)?);
    if head.is_none() {
        files.extend(split_nul(&git(
            dir,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?));
    }
    Ok(files)
}

fn split_nul(bytes: &[u8]) -> BTreeSet<PathBuf> {
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(path_from_bytes)
        .collect()
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
    PathBuf::from(OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// A file's contents at `rev`, or `None` if no file exists at that path there. An unknown
/// `rev` is an error rather than `None`, so a bad range cannot pass for "file was added".
pub fn show(dir: &Path, rev: &str, path: &Path) -> Result<Option<String>> {
    if !rev_exists(dir, rev) {
        return Err(Error::Git(format!("unknown revision `{rev}`")));
    }
    let spec = format!("{rev}:./{}", path.display());
    let out = output(dir, &["cat-file", "blob", &spec])?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// The branch `origin/HEAD` points at, if the remote has one.
pub fn default_branch(dir: &Path) -> Option<String> {
    git(
        dir,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .ok()
    .map(text)
    .and_then(|r| r.strip_prefix("origin/").map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sh(dir: &Path, args: &[&str]) -> String {
        text(git(dir, args).unwrap())
    }

    fn configure(dir: &Path) {
        for args in [
            &["config", "user.email", "t@axonal.dev"][..],
            &["config", "user.name", "axonal"],
            &["config", "commit.gpgsign", "false"],
            &["config", "core.hooksPath", "/dev/null"],
        ] {
            sh(dir, args);
        }
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        sh(dir.path(), &["init", "-q", "-b", "main"]);
        configure(dir.path());
        fs::create_dir_all(dir.path().join("ws")).unwrap();
        fs::write(dir.path().join("ws/a.txt"), "one").unwrap();
        fs::write(dir.path().join("top.txt"), "top").unwrap();
        commit(dir.path(), "initial");
        dir
    }

    fn commit(dir: &Path, message: &str) {
        sh(dir, &["add", "-A"]);
        sh(dir, &["commit", "-q", "-m", message]);
    }

    #[test]
    fn changed_files_cover_commits_worktree_and_untracked() {
        let dir = repo();
        let ws = dir.path().join("ws");
        let base = sh(dir.path(), &["rev-parse", "HEAD"]);
        sh(dir.path(), &["checkout", "-q", "-b", "feature"]);
        fs::write(ws.join("a.txt"), "two").unwrap();
        fs::write(dir.path().join("top.txt"), "changed outside the workspace").unwrap();
        commit(dir.path(), "change");
        assert_eq!(
            changed_files(&ws, &base, Some("HEAD")).unwrap(),
            BTreeSet::from([PathBuf::from("a.txt")])
        );
        fs::write(ws.join("new.txt"), "untracked").unwrap();
        fs::write(dir.path().join("outside.txt"), "untracked outside").unwrap();
        assert_eq!(
            changed_files(&ws, &base, None).unwrap(),
            BTreeSet::from([PathBuf::from("a.txt"), PathBuf::from("new.txt")])
        );
    }

    #[test]
    fn changed_files_include_deletions_and_staged_changes() {
        let dir = repo();
        let ws = dir.path().join("ws");
        let base = sh(dir.path(), &["rev-parse", "HEAD"]);
        fs::write(ws.join("b.txt"), "staged").unwrap();
        sh(dir.path(), &["add", "ws/b.txt"]);
        fs::remove_file(ws.join("a.txt")).unwrap();
        assert_eq!(
            changed_files(&ws, &base, None).unwrap(),
            BTreeSet::from([PathBuf::from("a.txt"), PathBuf::from("b.txt")])
        );
    }

    #[test]
    fn default_base_is_the_merge_base_with_the_default_branch() {
        let dir = repo();
        let main = sh(dir.path(), &["rev-parse", "HEAD"]);
        sh(dir.path(), &["checkout", "-q", "-b", "feature"]);
        fs::write(dir.path().join("ws/a.txt"), "two").unwrap();
        commit(dir.path(), "change");
        assert_eq!(default_base(dir.path(), "main", "HEAD").unwrap(), main);
        let err = default_base(dir.path(), "trunk", "HEAD").unwrap_err();
        assert!(err.to_string().contains("pass --base"), "{err}");
    }

    #[test]
    fn default_base_falls_back_to_the_remote_branch() {
        let origin = repo();
        let main = sh(origin.path(), &["rev-parse", "HEAD"]);
        let parent = tempfile::tempdir().unwrap();
        let clone = parent.path().join("clone");
        sh(
            parent.path(),
            &["clone", "-q", origin.path().to_str().unwrap(), "clone"],
        );
        configure(&clone);
        sh(&clone, &["checkout", "-q", "-b", "feature"]);
        sh(&clone, &["branch", "-q", "-D", "main"]);
        fs::write(clone.join("ws/a.txt"), "two").unwrap();
        commit(&clone, "change");
        assert_eq!(default_branch(&clone).as_deref(), Some("main"));
        assert_eq!(default_base(&clone, "main", "HEAD").unwrap(), main);
    }

    #[test]
    fn show_reads_old_contents_relative_to_the_workspace() {
        let dir = repo();
        let ws = dir.path().join("ws");
        let base = sh(dir.path(), &["rev-parse", "HEAD"]);
        fs::write(ws.join("a.txt"), "two").unwrap();
        commit(dir.path(), "change");
        assert_eq!(
            show(&ws, &base, Path::new("a.txt")).unwrap().as_deref(),
            Some("one")
        );
        assert_eq!(show(&ws, &base, Path::new("missing.txt")).unwrap(), None);
        assert_eq!(show(dir.path(), &base, Path::new("ws")).unwrap(), None);
        assert!(show(&ws, "no-such-rev", Path::new("a.txt")).is_err());
    }

    #[test]
    fn toplevel_and_default_branch() {
        let dir = repo();
        let top = toplevel(&dir.path().join("ws")).unwrap();
        assert_eq!(
            top.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
        assert_eq!(default_branch(dir.path()), None);
    }
}
