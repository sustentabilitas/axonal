//! Task cache keys: blake3 over command, config, input contents, the matching files of
//! dependency projects, env, dependency task keys and toolchain versions.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
    process::Command,
    time::UNIX_EPOCH,
};

use serde::{Deserialize, Serialize};

use crate::{
    config::DepsUsage,
    error::Result,
    files::{Patterns, display_root},
    graph::{Kind, Project, Target, TaskGraph, TaskId, Workspace},
};

const FILE_HASHES: &str = ".axonal/filehash.json";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Key(pub String);

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Content hashes keyed by path, reused while a file's size and mtime are unchanged.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FileHashCache {
    entries: BTreeMap<PathBuf, Stamp>,
    #[serde(skip)]
    dirty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stamp {
    size: u64,
    mtime_ns: u64,
    hash: String,
}

impl FileHashCache {
    /// A missing or unreadable cache file is an empty cache.
    pub fn load(root: &Path) -> Self {
        std::fs::read(root.join(FILE_HASHES))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let path = root.join(FILE_HASHES);
        std::fs::create_dir_all(path.parent().expect("cache file has a parent"))?;
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(
            &tmp,
            serde_json::to_vec(self).expect("file hashes serialize"),
        )?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    /// `rel` is workspace-relative.
    pub fn hash(&mut self, root: &Path, rel: &Path) -> Result<String> {
        let meta = std::fs::metadata(root.join(rel))?;
        let size = meta.len();
        let mtime_ns = meta
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        if let Some(stamp) = self
            .entries
            .get(rel)
            .filter(|s| s.size == size && s.mtime_ns == mtime_ns)
        {
            return Ok(stamp.hash.clone());
        }
        let hash = blake3::hash(&std::fs::read(root.join(rel))?)
            .to_hex()
            .to_string();
        self.entries.insert(
            rel.to_path_buf(),
            Stamp {
                size,
                mtime_ns,
                hash: hash.clone(),
            },
        );
        self.dirty = true;
        Ok(hash)
    }
}

/// Versions of the tools task commands use, probed once per run.
#[derive(Debug, Clone, Default)]
pub struct Toolchain {
    versions: BTreeMap<&'static str, String>,
}

impl Toolchain {
    /// Node and pnpm for JS workspaces, rustc for Cargo ones. A tool that is missing or
    /// fails hashes as `missing`.
    pub fn detect(ws: &Workspace) -> Toolchain {
        let uses = |kind: Kind, manifest: &str| {
            ws.root.join(manifest).is_file()
                || ws.projects.values().any(|p| p.kinds.contains(&kind))
        };
        let js: &[(&'static str, &'static [&'static str])] =
            &[("node", &["--version"]), ("pnpm", &["--version"])];
        let cargo: &[(&'static str, &'static [&'static str])] = &[("rustc", &["-vV"])];
        Toolchain {
            versions: [
                (uses(Kind::Js, "package.json"), js),
                (uses(Kind::Cargo, "Cargo.toml"), cargo),
            ]
            .into_iter()
            .filter(|(used, _)| *used)
            .flat_map(|(_, probes)| probes)
            .map(|(tool, args)| (*tool, probe(tool, args)))
            .collect(),
        }
    }

    /// Explicit projects may run anything, so they depend on every probed tool.
    fn relevant(&self, kinds: &BTreeSet<Kind>) -> Vec<(&'static str, &str)> {
        self.versions
            .iter()
            .filter(|(tool, _)| {
                kinds.contains(&Kind::Explicit)
                    || match **tool {
                        "rustc" => kinds.contains(&Kind::Cargo),
                        _ => kinds.contains(&Kind::Js),
                    }
            })
            .map(|(tool, version)| (*tool, version.as_str()))
            .collect()
    }
}

fn probe(tool: &str, args: &[&str]) -> String {
    Command::new(tool)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "missing".into())
}

/// Looks up an environment variable; production callers pass `std::env::var_os`.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// Cache keys for every task in `graph`, computed dependencies first.
pub fn task_keys(
    ws: &Workspace,
    graph: &TaskGraph,
    files: &mut FileHashCache,
    tools: &Toolchain,
    env: Env<'_>,
) -> Result<BTreeMap<TaskId, Key>> {
    graph
        .order
        .iter()
        .try_fold(BTreeMap::new(), |mut keys, id| {
            let key = hash_task(ws, id, &graph.deps[id], &keys, files, tools, env)?;
            keys.insert(id.clone(), key);
            Ok(keys)
        })
}

fn hash_task(
    ws: &Workspace,
    id: &TaskId,
    deps: &BTreeSet<TaskId>,
    keys: &BTreeMap<TaskId, Key>,
    files: &mut FileHashCache,
    tools: &Toolchain,
    env: Env<'_>,
) -> Result<Key> {
    let project = &ws.projects[&id.project];
    let target = ws.target(id);
    let mut fields = Fields::new();
    fields
        .add("format", "axonal-task-v1")
        .add("project", &project.name)
        .add("root", display_root(&project.root))
        .add("target", &id.target)
        .add(
            "config",
            serde_json::to_string(target).expect("targets serialize"),
        );
    for path in input_files(ws, project, target)? {
        let hash = files.hash(&ws.root, &path)?;
        fields.add("input", path_bytes(&path)).add("content", hash);
    }
    if target.deps_usage != DepsUsage::None {
        let patterns = Patterns::new(&target.inputs)?;
        for name in ws.dependency_closure(&project.name) {
            let dep = &ws.projects[name];
            fields.add("dep_project", name);
            for path in owned_matches(dep, &patterns) {
                let hash = files.hash(&ws.root, path)?;
                fields
                    .add("dep_input", path_bytes(path))
                    .add("content", hash);
            }
        }
    }
    for var in target.env.iter().collect::<BTreeSet<_>>() {
        fields.add("env", var);
        match env(var) {
            Some(value) => fields.add("set", value.as_encoded_bytes()),
            None => fields.add("unset", ""),
        };
    }
    for dep in deps {
        fields.add("dep", &keys[dep].0);
    }
    for (tool, version) in tools.relevant(&project.kinds) {
        fields.add(tool, version);
    }
    fields
        .add("os", std::env::consts::OS)
        .add("arch", std::env::consts::ARCH);
    Ok(Key(fields.finish()))
}

/// Length-prefixed fields, so no two different field lists hash the same bytes.
struct Fields(blake3::Hasher);

impl Fields {
    fn new() -> Self {
        Self(blake3::Hasher::new())
    }

    fn add(&mut self, name: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> &mut Self {
        for part in [name.as_ref(), value.as_ref()] {
            self.0.update(&(part.len() as u64).to_le_bytes());
            self.0.update(part);
        }
        self
    }

    fn finish(&self) -> String {
        self.0.finalize().to_hex().to_string()
    }
}

/// A path's platform bytes, so distinct non-UTF-8 paths never hash alike.
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_encoded_bytes()
}

/// Files `project` owns that match the project-relative globs. Only owned files, so a
/// project at the workspace root never sees its nested projects' files.
fn owned_matches<'a>(
    project: &'a Project,
    patterns: &'a Patterns,
) -> impl Iterator<Item = &'a PathBuf> {
    project.files.iter().filter(|f| {
        f.strip_prefix(&project.root)
            .is_ok_and(|rel| patterns.matches_project(rel))
    })
}

/// Owned files matching the target's project globs, plus any workspace file matching its
/// `{workspace}/` globs. Workspace-relative and sorted.
pub fn input_files(ws: &Workspace, project: &Project, target: &Target) -> Result<Vec<PathBuf>> {
    let patterns = Patterns::new(&target.inputs)?;
    let shared = ws.files.iter().filter(|f| patterns.matches_workspace(f));
    Ok(owned_matches(project, &patterns)
        .chain(shared)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    const CONFIG: &str = r#"
[targets.build]
depends_on = ["^build"]
inputs = ["src/**", "{workspace}/shared.cfg"]
env = ["AXONAL_HASH_TEST_MODE"]

[projects."libs/a".targets.build]
command = "make a"

[projects."apps/b"]
deps = ["libs/a"]

[projects."apps/b".targets.build]
command = "make b"

[projects."libs/a".targets.test]
command = "test a"

[projects."apps/b".targets.test]
command = "test b"
inputs = ["src/**"]

[projects."apps/b".targets.fmt]
command = "fmt b"
"#;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "axonal.toml", CONFIG);
        write(dir.path(), "libs/a/src/a.txt", "a");
        write(dir.path(), "libs/a/README.md", "docs");
        write(dir.path(), "apps/b/src/b.txt", "b");
        write(dir.path(), "shared.cfg", "x");
        dir
    }

    fn keys_with(root: &Path, env: &dyn Fn(&str) -> Option<OsString>) -> (String, String) {
        let ws = Workspace::discover(root).unwrap();
        let graph = TaskGraph::build(&ws, &["build".into()], None).unwrap();
        let keys = task_keys(
            &ws,
            &graph,
            &mut FileHashCache::default(),
            &Toolchain::default(),
            env,
        )
        .unwrap();
        (
            keys[&TaskId::new("libs/a", "build")].0.clone(),
            keys[&TaskId::new("apps/b", "build")].0.clone(),
        )
    }

    fn keys(root: &Path) -> (String, String) {
        keys_with(root, &no_env)
    }

    #[test]
    fn keys_are_stable() {
        let dir = fixture();
        assert_eq!(keys(dir.path()), keys(dir.path()));
    }

    #[test]
    fn input_changes_propagate_to_dependents() {
        let dir = fixture();
        let (a, b) = keys(dir.path());
        write(dir.path(), "libs/a/src/a.txt", "a2");
        let (a2, b2) = keys(dir.path());
        assert_ne!(a, a2);
        assert_ne!(b, b2);
    }

    fn key_with(
        root: &Path,
        target: &str,
        project: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> String {
        let ws = Workspace::discover(root).unwrap();
        let graph = TaskGraph::build(&ws, &[target.into()], None).unwrap();
        let keys = task_keys(
            &ws,
            &graph,
            &mut FileHashCache::default(),
            &Toolchain::default(),
            env,
        )
        .unwrap();
        keys[&TaskId::new(project, target)].0.clone()
    }

    fn key_of(root: &Path, target: &str, project: &str) -> String {
        key_with(root, target, project, &no_env)
    }

    #[test]
    fn dependency_inputs_change_keys_without_depends_on() {
        let dir = fixture();
        let (test, fmt) = (
            key_of(dir.path(), "test", "apps/b"),
            key_of(dir.path(), "fmt", "apps/b"),
        );
        write(
            dir.path(),
            "libs/a/README.md",
            "outside apps/b:test's inputs",
        );
        assert_eq!(key_of(dir.path(), "test", "apps/b"), test);
        write(dir.path(), "libs/a/src/a.txt", "a2");
        assert_ne!(key_of(dir.path(), "test", "apps/b"), test);
        assert_eq!(
            key_of(dir.path(), "fmt", "apps/b"),
            fmt,
            "fmt has deps_usage = none"
        );
    }

    #[test]
    fn non_inputs_do_not_change_keys() {
        let dir = fixture();
        let before = keys(dir.path());
        write(dir.path(), "libs/a/README.md", "new docs");
        assert_eq!(before, keys(dir.path()));
    }

    #[test]
    fn workspace_inputs_change_every_key() {
        let dir = fixture();
        let (a, b) = keys(dir.path());
        write(dir.path(), "shared.cfg", "y");
        let (a2, b2) = keys(dir.path());
        assert_ne!(a, a2);
        assert_ne!(b, b2);
    }

    fn mode(value: OsString) -> impl Fn(&str) -> Option<OsString> {
        move |name| (name == "AXONAL_HASH_TEST_MODE").then(|| value.clone())
    }

    #[test]
    fn listed_env_vars_change_keys() {
        let dir = fixture();
        let unset = keys(dir.path());
        let empty = keys_with(dir.path(), &mode("".into()));
        let ci = keys_with(dir.path(), &mode("ci".into()));
        assert_ne!(unset, empty, "unset differs from empty");
        assert_ne!(empty, ci);
        assert_eq!(ci, keys_with(dir.path(), &mode("ci".into())));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_env_values_are_hashed_as_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let dir = fixture();
        let bytes = |b: &[u8]| mode(std::ffi::OsStr::from_bytes(b).to_os_string());
        let one = keys_with(dir.path(), &bytes(b"\xff1"));
        assert_ne!(one, keys_with(dir.path(), &bytes(b"\xff2")));
        assert_ne!(one, keys(dir.path()));
    }

    #[test]
    fn unusual_env_names_do_not_panic_with_the_process_env() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "axonal.toml",
            "[projects.\"libs/a\".targets.build]\ncommand = \"make\"\nenv = [\"\", \"A=B\", \"X\\u0000Y\"]\n",
        );
        write(dir.path(), "libs/a/src/a.txt", "a");
        key_with(dir.path(), "build", "libs/a", &|name| {
            std::env::var_os(name)
        });
    }

    #[cfg(unix)]
    #[test]
    fn paths_are_hashed_as_raw_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let digest = |name: &[u8]| {
            let mut fields = Fields::new();
            fields.add(
                "input",
                path_bytes(Path::new(std::ffi::OsStr::from_bytes(name))),
            );
            fields.finish()
        };
        assert_ne!(digest(b"src/\xff"), digest(b"src/\xfe"));
    }

    #[test]
    fn input_files_combine_owned_and_workspace_globs() {
        let dir = fixture();
        let ws = Workspace::discover(dir.path()).unwrap();
        let b = &ws.projects["apps/b"];
        assert_eq!(
            input_files(&ws, b, &b.targets["build"]).unwrap(),
            vec![
                PathBuf::from("apps/b/src/b.txt"),
                PathBuf::from("shared.cfg")
            ]
        );
    }

    #[test]
    fn root_project_hashes_only_its_owned_files() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "axonal.toml",
            r#"
[projects.".".targets.build]
command = "make root"

[projects."libs/a".targets.build]
command = "make a"
"#,
        );
        write(dir.path(), "README.md", "root");
        write(dir.path(), "libs/a/src/a.txt", "a");
        let ws = Workspace::discover(dir.path()).unwrap();
        let root = &ws.projects["."];
        assert_eq!(
            input_files(&ws, root, &root.targets["build"]).unwrap(),
            vec![PathBuf::from("README.md"), PathBuf::from("axonal.toml")]
        );
        let before = key_of(dir.path(), "build", ".");
        write(dir.path(), "libs/a/src/a.txt", "a2");
        assert_eq!(key_of(dir.path(), "build", "."), before);
        write(dir.path(), "README.md", "root2");
        assert_ne!(key_of(dir.path(), "build", "."), before);
    }

    #[test]
    fn file_hashes_are_reused_while_size_and_mtime_match() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        write(dir.path(), "f.txt", "one");
        let mut cache = FileHashCache::default();
        let first = cache.hash(dir.path(), Path::new("f.txt")).unwrap();
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, "two").unwrap();
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(cache.hash(dir.path(), Path::new("f.txt")).unwrap(), first);

        cache.save(dir.path()).unwrap();
        let mut reloaded = FileHashCache::load(dir.path());
        assert_eq!(
            reloaded.hash(dir.path(), Path::new("f.txt")).unwrap(),
            first
        );
        fs::write(&file, "three").unwrap();
        assert_ne!(
            reloaded.hash(dir.path(), Path::new("f.txt")).unwrap(),
            first
        );
    }
}
