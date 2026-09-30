//! Workspace files: gitignore-aware listing, project ownership, and input/output globs.

use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::{DirEntry, WalkBuilder};

use crate::{
    config,
    error::{Error, Result},
};

/// Prefix that makes a glob relative to the workspace root instead of the project root.
pub const WORKSPACE_PREFIX: &str = "{workspace}/";

const SKIPPED_DIRS: [&str; 3] = [".git", ".axonal", "node_modules"];

fn is_skipped(entry: &DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .is_some_and(|n| SKIPPED_DIRS.contains(&n))
}

/// Every file and symlink under `root` not ignored by the repo's own `.gitignore` files
/// (global excludes, `.git/info/exclude` and `.ignore` files are not consulted), relative
/// to `root` and sorted. Symlinks are listed, never followed, so directory links can't
/// loop.
pub fn list(root: &Path) -> Result<Vec<PathBuf>> {
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .filter_entry(|e| !is_skipped(e))
        .build();
    let mut files = walker
        .filter_map(|entry| match entry {
            Ok(e) if e.file_type().is_some_and(|t| t.is_file() || t.is_symlink()) => {
                Some(Ok(e.into_path()))
            }
            Ok(_) => None,
            Err(err) => Some(Err(Error::Io(std::io::Error::other(err)))),
        })
        .map(|path| {
            path.map(|p| {
                p.strip_prefix(root)
                    .expect("walk stays under root")
                    .to_path_buf()
            })
        })
        .collect::<Result<Vec<_>>>()?;
    files.sort();
    Ok(files)
}

/// Maps workspace-relative paths to the deepest project root containing them.
#[derive(Debug, Clone, Default)]
pub struct Owners {
    roots: BTreeSet<PathBuf>,
}

impl Owners {
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            roots: roots.into_iter().collect(),
        }
    }

    pub fn owner(&self, path: &Path) -> Option<&Path> {
        path.ancestors()
            .find_map(|a| self.roots.get(a))
            .map(PathBuf::as_path)
    }
}

/// Lexically resolves `.` and `..`; `None` if the path is absolute or escapes the root.
pub fn normalize(path: &Path) -> Option<PathBuf> {
    path.components().try_fold(PathBuf::new(), |mut acc, c| {
        match c {
            Component::Normal(part) => acc.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !acc.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
        Some(acc)
    })
}

/// `.` for the workspace root, else the relative path.
pub fn display_root(root: &Path) -> String {
    if root.as_os_str().is_empty() {
        ".".into()
    } else {
        root.to_string_lossy().into_owned()
    }
}

/// A target's `inputs` or `outputs`, split into project-relative and `{workspace}/` globs.
#[derive(Debug, Clone)]
pub struct Patterns {
    project: GlobSet,
    workspace: GlobSet,
    project_globs: Vec<String>,
    workspace_globs: Vec<String>,
}

impl Patterns {
    pub fn new(globs: &[String]) -> Result<Self> {
        let (workspace_globs, project_globs): (Vec<String>, Vec<String>) =
            globs
                .iter()
                .try_fold((Vec::new(), Vec::new()), |(mut ws, mut proj), g| {
                    let rest = g.strip_prefix(WORKSPACE_PREFIX);
                    let rel = rest.unwrap_or(g);
                    if Path::new(rel).is_absolute() || rel.split('/').any(|seg| seg == "..") {
                        return Err(Error::Config {
                            path: PathBuf::from(config::FILE),
                            message: format!("glob `{g}` must not be absolute or contain `..`"),
                        });
                    }
                    match rest {
                        Some(rest) => ws.push(rest.to_string()),
                        None => proj.push(g.clone()),
                    }
                    Ok((ws, proj))
                })?;
        Ok(Self {
            project: glob_set(&project_globs)?,
            workspace: glob_set(&workspace_globs)?,
            project_globs,
            workspace_globs,
        })
    }

    /// `path` is relative to the project root.
    pub fn matches_project(&self, path: &Path) -> bool {
        self.project.is_match(path)
    }

    /// `path` is relative to the workspace root.
    pub fn matches_workspace(&self, path: &Path) -> bool {
        self.workspace.is_match(path)
    }

    pub fn has_workspace_globs(&self) -> bool {
        !self.workspace_globs.is_empty()
    }

    /// Project-relative globs without metacharacters: each names a single path.
    pub fn project_literals(&self) -> impl Iterator<Item = &str> {
        literals(&self.project_globs)
    }

    /// `{workspace}/` globs (prefix stripped) without metacharacters.
    pub fn workspace_literals(&self) -> impl Iterator<Item = &str> {
        literals(&self.workspace_globs)
    }

    /// Existing non-directories matching these globs, found by walking each glob's literal
    /// base without gitignore filtering (outputs are usually ignored) and skipping `.git`,
    /// `.axonal` and `node_modules` below it. Symlinks are reported, never followed: a
    /// matching link is a leaf, and a symlink on the path to the glob base, the project
    /// root included, is reported whether or not it matches, since everything the glob
    /// could match lies behind it.
    /// Workspace-relative and sorted.
    pub fn existing_files(&self, root: &Path, project_root: &Path) -> Result<Vec<PathBuf>> {
        [
            (&self.project_globs, &self.project, project_root),
            (&self.workspace_globs, &self.workspace, Path::new("")),
        ]
        .into_iter()
        .flat_map(|(globs, set, base)| globs.iter().map(move |glob| (glob, set, base)))
        .try_fold(BTreeSet::new(), |mut found, (glob, set, base)| {
            let walk = Walk { root, base, set };
            walk.start(&glob_base(glob), &mut found)?;
            Ok::<_, Error>(found)
        })
        .map(|found| found.into_iter().collect())
    }
}

/// A symlink-free walk collecting workspace-relative paths that match `set` relative to
/// `base` (itself workspace-relative).
struct Walk<'a> {
    root: &'a Path,
    base: &'a Path,
    set: &'a GlobSet,
}

impl Walk<'_> {
    /// Descends from the workspace root through `base` and `glob_base` one component at a
    /// time, stopping at the first missing path or symlink, then walks what it reached.
    fn start(&self, glob_base: &Path, found: &mut BTreeSet<PathBuf>) -> Result<()> {
        let mut rel = PathBuf::new();
        let mut parts = self
            .base
            .components()
            .chain(glob_base.components())
            .peekable();
        while let Some(part) = parts.next() {
            rel.push(part);
            let Some(meta) = symlink_metadata(&self.root.join(&rel))? else {
                return Ok(());
            };
            if meta.is_symlink() {
                found.insert(rel);
                return Ok(());
            }
            if !meta.is_dir() {
                if parts.peek().is_none() {
                    self.leaf(rel, found);
                }
                return Ok(());
            }
        }
        match symlink_metadata(&self.root.join(&rel))? {
            Some(meta) if meta.is_dir() => self.dir(&rel, found),
            _ => Ok(()),
        }
    }

    fn dir(&self, rel: &Path, found: &mut BTreeSet<PathBuf>) -> Result<()> {
        std::fs::read_dir(self.root.join(rel))?.try_for_each(|entry| {
            let entry = entry?;
            let path = rel.join(entry.file_name());
            if !entry.file_type()?.is_dir() {
                self.leaf(path, found);
                Ok(())
            } else if SKIPPED_DIRS.iter().any(|d| entry.file_name() == *d) {
                Ok(())
            } else {
                self.dir(&path, found)
            }
        })
    }

    fn leaf(&self, rel: PathBuf, found: &mut BTreeSet<PathBuf>) {
        if rel
            .strip_prefix(self.base)
            .is_ok_and(|r| self.set.is_match(r))
        {
            found.insert(rel);
        }
    }
}

fn symlink_metadata(path: &Path) -> Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn literals(globs: &[String]) -> impl Iterator<Item = &str> {
    globs
        .iter()
        .map(String::as_str)
        .filter(|g| !g.contains(['*', '?', '[', ']', '{', '}', '\\']))
}

fn glob_set(globs: &[String]) -> Result<GlobSet> {
    let invalid = |glob: &str, e: &dyn std::fmt::Display| Error::Config {
        path: PathBuf::from(config::FILE),
        message: format!("invalid glob `{glob}`: {e}"),
    };
    globs
        .iter()
        .try_fold(GlobSetBuilder::new(), |mut set, g| {
            set.add(
                GlobBuilder::new(g)
                    .literal_separator(true)
                    .build()
                    .map_err(|e| invalid(g, &e))?,
            );
            Ok::<_, Error>(set)
        })?
        .build()
        .map_err(|e| invalid(&globs.join(", "), &e))
}

/// The literal directory prefix of a glob: `dist/**/*.js` → `dist`.
pub fn glob_base(glob: &str) -> PathBuf {
    glob.split('/')
        .take_while(|seg| !seg.contains(['*', '?', '[', '{']))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, rel).unwrap();
    }

    fn paths(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn list_honours_gitignore_and_keeps_hidden_files() {
        let dir = tempfile::tempdir().unwrap();
        for f in [
            ".eslintrc.json",
            "src/a.ts",
            "dist/a.js",
            "node_modules/x/index.js",
            ".git/HEAD",
            ".axonal/cache/k.json",
        ] {
            touch(dir.path(), f);
        }
        fs::write(dir.path().join(".gitignore"), "dist/\n").unwrap();
        assert_eq!(
            list(dir.path()).unwrap(),
            paths(&[".eslintrc.json", ".gitignore", "src/a.ts"])
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_keeps_symlinks_without_following_directory_links() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "real/a.txt");
        symlink("real/a.txt", dir.path().join("file-link")).unwrap();
        symlink("real", dir.path().join("dir-link")).unwrap();
        symlink(".", dir.path().join("real/loop")).unwrap();
        symlink("nowhere", dir.path().join("dangling")).unwrap();
        assert_eq!(
            list(dir.path()).unwrap(),
            paths(&[
                "dangling",
                "dir-link",
                "file-link",
                "real/a.txt",
                "real/loop"
            ])
        );
    }

    #[test]
    fn owner_is_the_deepest_root() {
        let owners = Owners::new(paths(&["", "apps", "apps/web"]));
        assert_eq!(
            owners.owner(Path::new("apps/web/src/a.ts")),
            Some(Path::new("apps/web"))
        );
        assert_eq!(
            owners.owner(Path::new("apps/api.ts")),
            Some(Path::new("apps"))
        );
        assert_eq!(owners.owner(Path::new("README.md")), Some(Path::new("")));
        assert_eq!(
            Owners::new(paths(&["libs/a"])).owner(Path::new("README.md")),
            None
        );
    }

    #[test]
    fn normalize_resolves_dots_and_rejects_escapes() {
        assert_eq!(
            normalize(Path::new("a/./b/../c")),
            Some(PathBuf::from("a/c"))
        );
        assert_eq!(normalize(Path::new(".")), Some(PathBuf::new()));
        assert_eq!(normalize(Path::new("../x")), None);
        assert_eq!(normalize(Path::new("/etc")), None);
    }

    #[test]
    fn display_root_names_the_workspace_root_dot() {
        assert_eq!(display_root(Path::new("")), ".");
        assert_eq!(display_root(Path::new("libs/a")), "libs/a");
    }

    #[test]
    fn patterns_split_project_and_workspace_globs() {
        let p = Patterns::new(&["src/**".into(), "{workspace}/pnpm-lock.yaml".into()]).unwrap();
        assert!(p.matches_project(Path::new("src/a/b.ts")));
        assert!(!p.matches_project(Path::new("test/a.ts")));
        assert!(p.matches_workspace(Path::new("pnpm-lock.yaml")));
        assert!(!p.matches_workspace(Path::new("src/a.ts")));
        assert!(p.has_workspace_globs());
        assert!(
            !Patterns::new(&["src/**".into()])
                .unwrap()
                .has_workspace_globs()
        );
    }

    #[test]
    fn literals_are_globs_without_metacharacters() {
        let p = Patterns::new(&[
            "src/**".into(),
            ".env.local".into(),
            "a/{b,c}".into(),
            "{workspace}/.npmrc".into(),
            "{workspace}/*.lock".into(),
        ])
        .unwrap();
        assert_eq!(p.project_literals().collect::<Vec<_>>(), [".env.local"]);
        assert_eq!(p.workspace_literals().collect::<Vec<_>>(), [".npmrc"]);
    }

    #[test]
    fn single_star_does_not_cross_directories() {
        let p = Patterns::new(&["*.json".into()]).unwrap();
        assert!(p.matches_project(Path::new("package.json")));
        assert!(!p.matches_project(Path::new("src/data.json")));
    }

    #[test]
    fn invalid_glob_is_a_config_error() {
        assert!(matches!(
            Patterns::new(&["src/[".into()]),
            Err(Error::Config { .. })
        ));
    }

    #[test]
    fn escaping_globs_are_config_errors() {
        for glob in ["../x/**", "{workspace}/../x", "/abs/**"] {
            assert!(
                matches!(Patterns::new(&[glob.into()]), Err(Error::Config { .. })),
                "{glob}"
            );
        }
    }

    #[test]
    fn existing_files_skip_tool_dirs_below_the_walk_start() {
        let dir = tempfile::tempdir().unwrap();
        for f in [
            "libs/a/dist/a.js",
            "libs/a/node_modules/x/i.js",
            "libs/a/node_modules/.cache/c.js",
            "libs/a/.axonal/cache/k.js",
        ] {
            touch(dir.path(), f);
        }
        let root = Path::new("libs/a");
        let all = Patterns::new(&["**/*.js".into()]).unwrap();
        assert_eq!(
            all.existing_files(dir.path(), root).unwrap(),
            paths(&["libs/a/dist/a.js"])
        );
        let cache = Patterns::new(&["node_modules/.cache/**".into()]).unwrap();
        assert_eq!(
            cache.existing_files(dir.path(), root).unwrap(),
            paths(&["libs/a/node_modules/.cache/c.js"])
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_files_report_symlink_leaves_without_following_them() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "libs/a/dist/a.js");
        touch(dir.path(), "elsewhere/creds/secret");
        symlink(
            dir.path().join("elsewhere/creds"),
            dir.path().join("libs/a/dist/config"),
        )
        .unwrap();
        symlink("a.js", dir.path().join("libs/a/dist/b.js")).unwrap();
        symlink("nowhere", dir.path().join("libs/a/dist/dangling")).unwrap();
        let p = Patterns::new(&["dist/**".into()]).unwrap();
        assert_eq!(
            p.existing_files(dir.path(), Path::new("libs/a")).unwrap(),
            paths(&[
                "libs/a/dist/a.js",
                "libs/a/dist/b.js",
                "libs/a/dist/config",
                "libs/a/dist/dangling"
            ])
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_files_never_follow_a_symlinked_glob_base() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "elsewhere/out/x.js");
        fs::create_dir_all(dir.path().join("libs/a")).unwrap();
        symlink(dir.path().join("elsewhere"), dir.path().join("libs/a/dist")).unwrap();
        for glob in ["dist/**", "dist/out/**/*.js"] {
            let p = Patterns::new(&[glob.into()]).unwrap();
            assert_eq!(
                p.existing_files(dir.path(), Path::new("libs/a")).unwrap(),
                paths(&["libs/a/dist"]),
                "{glob}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn existing_files_never_follow_a_symlinked_project_root() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "elsewhere/a/dist/x.js");
        fs::create_dir_all(dir.path().join("libs")).unwrap();
        symlink(dir.path().join("elsewhere/a"), dir.path().join("libs/a")).unwrap();
        symlink(dir.path().join("elsewhere"), dir.path().join("linked")).unwrap();
        for (project, expected) in [("libs/a", "libs/a"), ("linked/a", "linked")] {
            for glob in ["dist/**", "**/*"] {
                let p = Patterns::new(&[glob.into()]).unwrap();
                assert_eq!(
                    p.existing_files(dir.path(), Path::new(project)).unwrap(),
                    paths(&[expected]),
                    "{project} {glob}"
                );
            }
        }
    }

    #[test]
    fn glob_base_stops_at_the_first_wildcard() {
        assert_eq!(glob_base("dist/**/*.js"), PathBuf::from("dist"));
        assert_eq!(glob_base("**/*"), PathBuf::new());
        assert_eq!(glob_base("out/report.txt"), PathBuf::from("out/report.txt"));
    }

    #[test]
    fn existing_files_include_ignored_outputs() {
        let dir = tempfile::tempdir().unwrap();
        for f in [
            "libs/a/dist/x.js",
            "libs/a/dist/sub/y.js",
            "libs/a/src/a.ts",
            "coverage/a/lcov.info",
        ] {
            touch(dir.path(), f);
        }
        fs::write(dir.path().join(".gitignore"), "dist/\ncoverage/\n").unwrap();
        let p = Patterns::new(&["dist/**".into(), "{workspace}/coverage/a/**".into()]).unwrap();
        assert_eq!(
            p.existing_files(dir.path(), Path::new("libs/a")).unwrap(),
            paths(&[
                "coverage/a/lcov.info",
                "libs/a/dist/sub/y.js",
                "libs/a/dist/x.js"
            ])
        );
    }
}
