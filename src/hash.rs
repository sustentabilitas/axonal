//! Task cache keys: blake3 over command, config, input contents, the matching files of
//! dependency projects, env, dependency task keys and toolchain versions.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fmt,
    fs::{self, Metadata},
    io,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    config::DepsUsage,
    error::{Error, Result},
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

/// Bumped whenever [`Stamp`] or the digest format changes; other versions load empty.
const FILE_HASHES_VERSION: u32 = 2;
/// Files modified this recently may change again within the timestamp granularity, so
/// their stamps are never stored (git's "racy clean" rule).
const RACY_WINDOW: Duration = Duration::from_secs(2);
/// The digest of a path that doesn't exist.
const MISSING: &str = "missing";

/// Content hashes keyed by UTF-8 path, reused across runs while a file's [`Stat`] is
/// unchanged. Symlinks and non-UTF-8 paths are always hashed afresh.
#[derive(Debug, Default)]
pub struct FileHashCache {
    entries: BTreeMap<String, Stamp>,
    /// Paths hashed this run; `save` drops every other entry.
    seen: BTreeSet<String>,
    dirty: bool,
}

#[derive(Serialize, Deserialize)]
struct OnDisk<E> {
    version: u32,
    entries: E,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stat {
    size: u64,
    mtime_ns: u64,
    ctime_s: i64,
    ctime_ns: i64,
    ino: u64,
    dev: u64,
}

impl Stat {
    fn of(meta: &Metadata) -> Stat {
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        #[cfg(unix)]
        let (ctime_s, ctime_ns, ino, dev) = {
            use std::os::unix::fs::MetadataExt;
            (meta.ctime(), meta.ctime_nsec(), meta.ino(), meta.dev())
        };
        #[cfg(not(unix))]
        let (ctime_s, ctime_ns, ino, dev) = (0, 0, 0, 0);
        Stat {
            size: meta.len(),
            mtime_ns,
            ctime_s,
            ctime_ns,
            ino,
            dev,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    #[serde(flatten)]
    stat: Stat,
    hash: String,
}

/// One path's digest, plus the stamp to keep for it (`None`: keep nothing).
struct Hashed {
    digest: String,
    stamp: Option<Stamp>,
}

impl FileHashCache {
    /// A missing, unreadable or other-version cache file is an empty cache.
    pub fn load(root: &Path) -> Self {
        fs::read(root.join(FILE_HASHES))
            .ok()
            .and_then(|bytes| {
                serde_json::from_slice::<OnDisk<BTreeMap<String, Stamp>>>(&bytes).ok()
            })
            .filter(|disk| disk.version == FILE_HASHES_VERSION)
            .map(|disk| FileHashCache {
                entries: disk.entries,
                ..FileHashCache::default()
            })
            .unwrap_or_default()
    }

    /// Writes the entries hashed since `load`, if anything changed.
    pub fn save(&self, root: &Path) -> Result<()> {
        let kept: BTreeMap<&String, &Stamp> = self
            .entries
            .iter()
            .filter(|(path, _)| self.seen.contains(*path))
            .collect();
        if !self.dirty && kept.len() == self.entries.len() {
            return Ok(());
        }
        let bytes = serde_json::to_vec(&OnDisk {
            version: FILE_HASHES_VERSION,
            entries: kept,
        })
        .map_err(io::Error::other)?;
        let path = root.join(FILE_HASHES);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        fs::write(&tmp, bytes)?;
        fs::rename(tmp, path)?;
        Ok(())
    }

    /// The digest of `rel` (workspace-relative): its content and executable bit, a
    /// symlink's text and target, or `missing`.
    pub fn hash(&mut self, root: &Path, rel: &Path) -> Result<String> {
        let hashed = self.compute(root, rel)?;
        self.record(rel, hashed.stamp);
        Ok(hashed.digest)
    }

    fn compute(&self, root: &Path, rel: &Path) -> Result<Hashed> {
        let path = root.join(rel);
        let uncached = |digest: String| Hashed {
            digest,
            stamp: None,
        };
        let meta = match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(uncached(MISSING.into())),
            Err(e) => return Err(input_error(rel, e)),
            Ok(meta) => meta,
        };
        if meta.is_symlink() {
            return symlink_digest(&path, rel).map(uncached);
        }
        if meta.is_dir() {
            return Ok(uncached("dir".into()));
        }
        let stat = Stat::of(&meta);
        let cached = rel
            .to_str()
            .and_then(|key| self.entries.get(key))
            .filter(|stamp| stamp.stat == stat)
            .map(|stamp| stamp.hash.clone());
        let content = match cached.map_or_else(|| content_hash(&path), Ok) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(uncached(MISSING.into())),
            Err(e) => return Err(input_error(rel, e)),
            Ok(content) => content,
        };
        Ok(Hashed {
            digest: file_digest(&meta, &content),
            stamp: (!is_racy(&meta)).then_some(Stamp {
                stat,
                hash: content,
            }),
        })
    }

    fn record(&mut self, rel: &Path, stamp: Option<Stamp>) {
        let Some(key) = rel.to_str() else { return };
        self.seen.insert(key.to_string());
        match stamp {
            Some(stamp) if self.entries.get(key) != Some(&stamp) => {
                self.entries.insert(key.to_string(), stamp);
                self.dirty = true;
            }
            Some(_) => {}
            None => self.dirty |= self.entries.remove(key).is_some(),
        }
    }
}

fn input_error(rel: &Path, source: io::Error) -> Error {
    Error::Input {
        path: rel.to_path_buf(),
        source,
    }
}

fn content_hash(path: &Path) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(fs::File::open(path)?)?;
    Ok(hasher.finalize().to_hex().to_string())
}

fn file_digest(meta: &Metadata, content: &str) -> String {
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = {
        let _ = meta;
        false
    };
    format!("file:{}:{content}", if executable { "x" } else { "-" })
}

/// The link text plus what it resolves to: a file's digest, `dir`, or `dangling` (which
/// covers loops too).
fn symlink_digest(path: &Path, rel: &Path) -> Result<String> {
    let link = fs::read_link(path).map_err(|e| input_error(rel, e))?;
    let target = match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => "dir".to_string(),
        Ok(meta) => match content_hash(path) {
            Ok(content) => file_digest(&meta, &content),
            Err(e) if e.kind() == io::ErrorKind::NotFound => "dangling".into(),
            Err(e) => return Err(input_error(rel, e)),
        },
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Err(input_error(rel, e)),
        Err(_) => "dangling".into(),
    };
    let mut fields = Fields::new();
    fields.add("link", path_bytes(&link)).add("target", target);
    Ok(format!("link:{}", fields.finish()))
}

fn is_racy(meta: &Metadata) -> bool {
    meta.modified()
        .ok()
        .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
        .is_none_or(|age| age < RACY_WINDOW)
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

    const ONE: &str =
        "[projects.\"libs/a\".targets.build]\ncommand = \"make\"\noutputs = [\"out/**\"]\n";

    fn one_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "axonal.toml", ONE);
        write(dir.path(), "libs/a/src/a.txt", "a");
        dir
    }

    fn set_mtime(path: &Path, mtime: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    /// Moves the mtime out of the racy window so the stamp is cached.
    fn back_date(path: &Path) -> SystemTime {
        let mtime = SystemTime::now() - Duration::from_secs(60);
        set_mtime(path, mtime);
        mtime
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_inputs_track_the_link_and_its_target() {
        use std::os::unix::fs::symlink;
        let dir = one_project();
        write(dir.path(), "shared/config.json", "{\"v\":1}");
        write(dir.path(), "shared/other.json", "{\"v\":1}");
        let link = dir.path().join("libs/a/src/config.json");
        symlink("../../../shared/config.json", &link).unwrap();
        let ws = Workspace::discover(dir.path()).unwrap();
        assert!(
            ws.projects["libs/a"]
                .files
                .contains(&PathBuf::from("libs/a/src/config.json"))
        );
        let before = key_of(dir.path(), "build", "libs/a");
        write(dir.path(), "shared/config.json", "{\"v\":2}");
        let edited = key_of(dir.path(), "build", "libs/a");
        assert_ne!(before, edited, "editing the target changes the key");
        fs::remove_file(&link).unwrap();
        symlink("../../../shared/other.json", &link).unwrap();
        assert_ne!(
            key_of(dir.path(), "build", "libs/a"),
            edited,
            "retargeting changes the key"
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_and_dangling_links_hash_without_errors() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("d")).unwrap();
        symlink("d", dir.path().join("to-dir")).unwrap();
        symlink("nowhere", dir.path().join("dangling")).unwrap();
        let mut cache = FileHashCache::default();
        let to_dir = cache.hash(dir.path(), Path::new("to-dir")).unwrap();
        let dangling = cache.hash(dir.path(), Path::new("dangling")).unwrap();
        assert_ne!(to_dir, dangling);
        fs::remove_file(dir.path().join("dangling")).unwrap();
        symlink("d", dir.path().join("dangling")).unwrap();
        assert_ne!(
            cache.hash(dir.path(), Path::new("dangling")).unwrap(),
            dangling
        );
    }

    #[test]
    fn vanished_files_hash_as_missing() {
        let dir = one_project();
        let ws = Workspace::discover(dir.path()).unwrap();
        let graph = TaskGraph::build(&ws, &["build".into()], None).unwrap();
        let key = |ws: &Workspace| {
            task_keys(
                ws,
                &graph,
                &mut FileHashCache::default(),
                &Toolchain::default(),
                &no_env,
            )
            .unwrap()[&TaskId::new("libs/a", "build")]
                .clone()
        };
        let before = key(&ws);
        fs::remove_file(dir.path().join("libs/a/src/a.txt")).unwrap();
        assert_ne!(key(&ws), before);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_files_are_errors_naming_the_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = one_project();
        let file = dir.path().join("libs/a/src/a.txt");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&file).is_ok() {
            return; // running as root
        }
        let err = FileHashCache::default()
            .hash(dir.path(), Path::new("libs/a/src/a.txt"))
            .unwrap_err();
        assert!(matches!(err, Error::Input { .. }), "{err:?}");
        assert!(err.to_string().contains("libs/a/src/a.txt"), "{err}");
    }

    #[test]
    fn file_hashes_are_reused_while_the_stamp_matches() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "one");
        back_date(&dir.path().join("f.txt"));
        let mut cache = FileHashCache::default();
        let first = cache.hash(dir.path(), Path::new("f.txt")).unwrap();
        cache.save(dir.path()).unwrap();

        let mut reloaded = FileHashCache::load(dir.path());
        reloaded.entries.get_mut("f.txt").unwrap().hash = "sentinel".into();
        assert!(
            reloaded
                .hash(dir.path(), Path::new("f.txt"))
                .unwrap()
                .contains("sentinel"),
            "an unchanged stamp reuses the stored hash"
        );
        write(dir.path(), "f.txt", "three");
        let changed = reloaded.hash(dir.path(), Path::new("f.txt")).unwrap();
        assert!(!changed.contains("sentinel"));
        assert_ne!(changed, first);
    }

    #[test]
    fn same_size_rewrites_with_a_restored_mtime_are_rehashed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        write(dir.path(), "f.txt", "v1");
        let mtime = back_date(&file);
        let mut cache = FileHashCache::default();
        let first = cache.hash(dir.path(), Path::new("f.txt")).unwrap();
        cache.save(dir.path()).unwrap();
        assert!(
            FileHashCache::load(dir.path())
                .entries
                .contains_key("f.txt")
        );

        write(dir.path(), "f.txt", "v2");
        set_mtime(&file, mtime);
        let mut reloaded = FileHashCache::load(dir.path());
        assert_ne!(
            reloaded.hash(dir.path(), Path::new("f.txt")).unwrap(),
            first,
            "same size and mtime, newer ctime"
        );
    }

    #[test]
    fn racy_files_are_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "fresh");
        let mut cache = FileHashCache::default();
        cache.hash(dir.path(), Path::new("f.txt")).unwrap();
        cache.save(dir.path()).unwrap();
        assert!(
            !FileHashCache::load(dir.path())
                .entries
                .contains_key("f.txt")
        );
    }

    #[test]
    fn other_cache_versions_load_empty() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "one");
        back_date(&dir.path().join("f.txt"));
        let mut cache = FileHashCache::default();
        cache.hash(dir.path(), Path::new("f.txt")).unwrap();
        cache.save(dir.path()).unwrap();
        let path = dir.path().join(FILE_HASHES);
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        json["version"] = (FILE_HASHES_VERSION + 1).into();
        fs::write(&path, json.to_string()).unwrap();
        assert!(FileHashCache::load(dir.path()).entries.is_empty());
    }

    #[test]
    fn unseen_entries_are_pruned_on_save() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["a.txt", "b.txt"] {
            write(dir.path(), f, f);
            back_date(&dir.path().join(f));
        }
        let mut cache = FileHashCache::default();
        cache.hash(dir.path(), Path::new("a.txt")).unwrap();
        cache.hash(dir.path(), Path::new("b.txt")).unwrap();
        cache.save(dir.path()).unwrap();
        let mut second = FileHashCache::load(dir.path());
        second.hash(dir.path(), Path::new("a.txt")).unwrap();
        second.save(dir.path()).unwrap();
        let third = FileHashCache::load(dir.path());
        assert_eq!(third.entries.keys().collect::<Vec<_>>(), ["a.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_hash_but_are_not_cached() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let rel = Path::new(std::ffi::OsStr::from_bytes(b"\xff.txt"));
        // APFS rejects non-UTF-8 names; the path then hashes as missing.
        let created = fs::write(dir.path().join(rel), "x").is_ok();
        if created {
            back_date(&dir.path().join(rel));
        }
        let mut cache = FileHashCache::default();
        let digest = cache.hash(dir.path(), rel).unwrap();
        assert_eq!(digest == MISSING, !created);
        cache.save(dir.path()).unwrap();
        assert!(FileHashCache::load(dir.path()).entries.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_executable_bit_changes_keys() {
        use std::os::unix::fs::PermissionsExt;
        let dir = one_project();
        write(dir.path(), "libs/a/run.sh", "echo hi");
        let before = key_of(dir.path(), "build", "libs/a");
        fs::set_permissions(
            dir.path().join("libs/a/run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_ne!(key_of(dir.path(), "build", "libs/a"), before);
    }
}
