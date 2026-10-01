#![allow(dead_code)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Variables that point git at another repository; cleared so a hook running `cargo test`
/// can't redirect the temp repos into the developer's.
const REPO_VARS: [&str; 5] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_COMMON_DIR",
];

/// A fixture copied into a temp dir and committed as the first commit on `main`.
pub struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    pub fn fixture(name: &str) -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        copy_dir(&source, dir.path());
        let repo = Repo { dir };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.email", "tests@axonal.dev"]);
        repo.git(&["config", "user.name", "axonal tests"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo.git(&["config", "core.hooksPath", "/dev/null"]);
        repo.commit("initial");
        repo
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn write(&self, rel: &str, contents: &str) {
        let path = self.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    pub fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path().join(rel)).unwrap()
    }

    pub fn remove_dir(&self, rel: &str) {
        fs::remove_dir_all(self.path().join(rel)).unwrap();
    }

    pub fn git(&self, args: &[&str]) -> String {
        let mut git = Command::new("git");
        REPO_VARS.iter().for_each(|var| {
            git.env_remove(var);
        });
        let out = git.args(args).current_dir(self.path()).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    pub fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }

    /// Starts a feature branch, applies `change`, and commits it.
    pub fn feature(&self, change: impl FnOnce(&Repo)) {
        self.git(&["checkout", "-q", "-b", "feature"]);
        change(self);
        self.commit("feature change");
    }

    pub fn ax(&self) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::new(env!("CARGO_BIN_EXE_ax"));
        REPO_VARS.iter().for_each(|var| {
            cmd.env_remove(var);
        });
        cmd.current_dir(self.path());
        cmd
    }

    /// Runs axonal, asserts success, and returns stdout.
    pub fn stdout(&self, args: &[&str]) -> String {
        let out = self.ax().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "axonal {args:?} exited {:?}\nstdout:\n{}\nstderr:\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// Runs axonal and returns (exit code, stdout, stderr).
    pub fn output(&self, args: &[&str]) -> (i32, String, String) {
        let out = self.ax().args(args).output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
    }
}

fn copy_dir(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target: PathBuf = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}
