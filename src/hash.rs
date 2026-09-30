//! Task cache keys: blake3 over command, config, input contents, the matching files of
//! dependency projects, env, dependency task keys and toolchain versions.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, hash_map},
    ffi::OsString,
    fmt,
    fs::{self, Metadata},
    hash::Hash,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ignore::WalkBuilder;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    config::DepsUsage,
    error::{Error, Result},
    files::{self, Patterns, display_root},
    graph::{Kind, Project, Target, TaskGraph, TaskId, Workspace, infer::ts},
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
/// The digest of a FIFO, socket or device, which is never opened.
const SPECIAL: &str = "special";

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

    /// Writes the entries, if anything changed, dropping those whose paths no longer
    /// exist (entries for paths a filtered run didn't hash are kept).
    pub fn save(&self, root: &Path) -> Result<()> {
        let kept: BTreeMap<&String, &Stamp> = self
            .entries
            .par_iter()
            .filter(|(path, _)| {
                self.seen.contains(*path) || fs::symlink_metadata(root.join(path)).is_ok()
            })
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
    /// symlink's text and target (a directory's contents included), `special` for FIFOs,
    /// sockets and devices, or `missing`.
    pub fn hash(&mut self, root: &Path, rel: &Path) -> Result<String> {
        let hashed = self.compute(root, rel)?;
        self.record(rel, hashed.stamp);
        Ok(hashed.digest)
    }

    /// Digests of `paths`, hashed in parallel. If several fail, the error is the one for
    /// the smallest path.
    fn hash_all<'p>(
        &mut self,
        root: &Path,
        paths: Vec<&'p Path>,
    ) -> Result<HashMap<&'p Path, String>> {
        let (hashed, failed): (Vec<_>, Vec<_>) = paths
            .into_par_iter()
            .map(|path| (path, self.compute(root, path)))
            .collect::<Vec<_>>()
            .into_iter()
            .partition(|(_, hashed)| hashed.is_ok());
        if let Some((_, Err(error))) = failed.into_iter().min_by_key(|(path, _)| *path) {
            return Err(error);
        }
        Ok(hashed
            .into_iter()
            .filter_map(|(path, hashed)| hashed.ok().map(|hashed| (path, hashed)))
            .map(|(path, hashed)| {
                self.record(path, hashed.stamp);
                (path, hashed.digest)
            })
            .collect())
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
            return symlink_digest(&path, rel, Follow::Directories).map(uncached);
        }
        if meta.is_dir() {
            return Ok(uncached("dir".into()));
        }
        if !meta.is_file() {
            return Ok(uncached(SPECIAL.into()));
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

/// Opened non-blocking on Unix, so a file swapped for a FIFO after its stat can't hang.
fn content_hash(path: &Path) -> io::Result<String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(options.open(path)?)?;
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

/// Whether a symlink to a directory folds in the directory's contents.
#[derive(Clone, Copy)]
enum Follow {
    Directories,
    /// For links met inside a followed directory, so link loops can't recurse.
    Nothing,
}

/// Directories a followed directory link never descends into.
const UNWALKED_DIRS: [&str; 2] = [".git", "node_modules"];

/// The link text plus what it resolves to: a file's digest, a directory's contents (or
/// just `dir`), `special`, or `dangling` (which covers loops too).
fn symlink_digest(path: &Path, rel: &Path, follow: Follow) -> Result<String> {
    let link = fs::read_link(path).map_err(|e| input_error(rel, e))?;
    let target = match (fs::metadata(path), follow) {
        (Ok(meta), Follow::Directories) if meta.is_dir() => dir_digest(path, rel)?,
        (Ok(meta), Follow::Nothing) if meta.is_dir() => "dir".into(),
        (Ok(meta), _) if !meta.is_file() => SPECIAL.into(),
        (Ok(meta), _) => match content_hash(path) {
            Ok(content) => file_digest(&meta, &content),
            Err(e) if e.kind() == io::ErrorKind::NotFound => "dangling".into(),
            Err(e) => return Err(input_error(rel, e)),
        },
        (Err(e), _) if e.kind() == io::ErrorKind::PermissionDenied => {
            return Err(input_error(rel, e));
        }
        (Err(_), _) => "dangling".into(),
    };
    let mut fields = Fields::new();
    fields.add("link", path_bytes(&link)).add("target", target);
    Ok(format!("link:{}", fields.finish()))
}

/// Every entry under the directory `link` resolves to, sorted by relative path. Nested
/// links aren't followed into directories, and `.git` and `node_modules` are skipped.
fn dir_digest(link: &Path, rel: &Path) -> Result<String> {
    let base = fs::canonicalize(link).map_err(|e| input_error(rel, e))?;
    let walk_error = |e: ignore::Error| input_error(rel, io::Error::other(e));
    let mut entries = WalkBuilder::new(&base)
        .standard_filters(false)
        .follow_links(false)
        .filter_entry(|e| {
            e.depth() == 0
                || !(e.file_type().is_some_and(|t| t.is_dir())
                    && e.file_name()
                        .to_str()
                        .is_some_and(|n| UNWALKED_DIRS.contains(&n)))
        })
        .build()
        .filter(|entry| {
            entry
                .as_ref()
                .map_or(true, |e| !e.file_type().is_some_and(|t| t.is_dir()))
        })
        .map(|entry| {
            let path = entry.map_err(walk_error)?.into_path();
            let inner = path
                .strip_prefix(&base)
                .expect("walk stays under base")
                .to_path_buf();
            let digest = entry_digest(&path, &rel.join(&inner))?;
            Ok((inner, digest))
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort();
    let mut fields = Fields::new();
    entries.iter().for_each(|(inner, digest)| {
        fields.add(path_bytes(inner), digest);
    });
    Ok(format!("dir:{}", fields.finish()))
}

/// A path inside a followed directory: a file's digest, a link's digest (never followed
/// into directories), `special`, or `missing` if it vanished mid-walk.
fn entry_digest(path: &Path, rel: &Path) -> Result<String> {
    let meta = match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(MISSING.into()),
        Err(e) => return Err(input_error(rel, e)),
        Ok(meta) => meta,
    };
    if meta.is_symlink() {
        return symlink_digest(path, rel, Follow::Nothing);
    }
    if !meta.is_file() {
        return Ok(SPECIAL.into());
    }
    match content_hash(path) {
        Ok(content) => Ok(file_digest(&meta, &content)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(MISSING.into()),
        Err(e) => Err(input_error(rel, e)),
    }
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
    /// Node and pnpm for JS workspaces, rustc for Cargo ones, run in parallel from the
    /// workspace root (so `rust-toolchain.toml`, corepack and version managers apply). A
    /// tool that is missing or fails hashes as `missing`; one still running after
    /// five seconds is killed and hashes as `timeout`.
    pub fn detect(ws: &Workspace) -> Toolchain {
        let uses = |kind: Kind, manifest: &str| {
            ws.root.join(manifest).is_file()
                || ws.projects.values().any(|p| p.kinds.contains(&kind))
        };
        let probes: Vec<Probe> = [
            (uses(Kind::Js, "package.json"), &JS_PROBES[..]),
            (uses(Kind::Cargo, "Cargo.toml"), &CARGO_PROBES[..]),
        ]
        .into_iter()
        .filter(|(used, _)| *used)
        .flat_map(|(_, probes)| probes.iter().cloned())
        .collect();
        Toolchain {
            versions: run_probes(&ws.root, &probes, PROBE_TIMEOUT),
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

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_POLL: Duration = Duration::from_millis(10);

/// A version command; its trimmed stdout is hashed under `name`.
#[derive(Debug, Clone)]
struct Probe {
    name: &'static str,
    program: &'static str,
    args: &'static [&'static str],
}

const JS_PROBES: [Probe; 2] = [
    Probe {
        name: "node",
        program: "node",
        args: &["--version"],
    },
    Probe {
        name: "pnpm",
        program: "pnpm",
        args: &["--version"],
    },
];
const CARGO_PROBES: [Probe; 1] = [Probe {
    name: "rustc",
    program: "rustc",
    args: &["-vV"],
}];

fn run_probes(dir: &Path, probes: &[Probe], timeout: Duration) -> BTreeMap<&'static str, String> {
    std::thread::scope(|scope| {
        probes
            .iter()
            .map(|p| (p.name, scope.spawn(move || probe(dir, p, timeout))))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|(name, handle)| (name, handle.join().unwrap_or_else(|_| "missing".into())))
            .collect()
    })
}

/// Version output is small, so the child never blocks on a full stdout pipe before exit.
fn probe(dir: &Path, p: &Probe, timeout: Duration) -> String {
    let Ok(mut child) = Command::new(p.program)
        .args(p.args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return "missing".into();
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(None) if Instant::now() < deadline => std::thread::sleep(PROBE_POLL),
            Ok(None) => {
                // A failed kill means the child already exited; `wait` reaps it either way.
                let _ = child.kill();
                let _ = child.wait();
                return "timeout".into();
            }
            Ok(Some(status)) => break status,
            Err(_) => return "missing".into(),
        }
    };
    let mut stdout = String::new();
    match child
        .stdout
        .take()
        .map(|mut out| out.read_to_string(&mut stdout))
    {
        Some(Ok(_)) if status.success() => stdout.trim().to_string(),
        _ => "missing".into(),
    }
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
    let mut plan = Plan::with_closures(
        ws,
        graph
            .order
            .iter()
            .filter(|id| ws.target(id).deps_usage != DepsUsage::None)
            .map(|id| id.project.as_str())
            .collect(),
    );
    let tasks = graph
        .order
        .iter()
        .map(|id| plan.task(id))
        .collect::<Result<Vec<_>>>()?;
    let digests = files.hash_all(&ws.root, plan.files())?;
    let mut dep_digests: HashMap<&[String], HashMap<&str, blake3::Hash>> = HashMap::new();
    let mut closure_digests = HashMap::new();
    Ok(tasks.iter().fold(BTreeMap::new(), |mut keys, task| {
        let inputs = task.target.inputs.as_slice();
        let closure = task.closure.as_ref().map(|names| {
            *closure_digests
                .entry((task.project.name.as_str(), inputs))
                .or_insert_with(|| {
                    let mut fields = Fields::new();
                    let dep_digests = dep_digests.entry(inputs).or_default();
                    names.iter().for_each(|dep| {
                        let digest = dep_digests
                            .entry(*dep)
                            .or_insert_with(|| plan.dep_digest(dep, inputs, &digests));
                        fields
                            .add("dep_project", dep)
                            .add("dep_inputs", digest.as_bytes());
                    });
                    fields.hash()
                })
        });
        let key = hash_task(
            task,
            closure,
            &graph.deps[task.id],
            &keys,
            &digests,
            tools,
            env,
        );
        keys.insert(task.id.clone(), key);
        keys
    }))
}

fn hash_task(
    task: &TaskInputs<'_>,
    closure: Option<blake3::Hash>,
    deps: &BTreeSet<TaskId>,
    keys: &BTreeMap<TaskId, Key>,
    digests: &HashMap<&Path, String>,
    tools: &Toolchain,
    env: Env<'_>,
) -> Key {
    let TaskInputs {
        id,
        project,
        target,
        ..
    } = *task;
    let mut fields = Fields::new();
    fields
        .add("format", "axonal-task-v2")
        .add("project", &project.name)
        .add("root", display_root(&project.root))
        .add("target", &id.target)
        .add(
            "config",
            serde_json::to_string(target).expect("targets serialize"),
        );
    for path in &task.own {
        fields
            .add("input", path_bytes(path))
            .add("content", &digests[path.as_path()]);
    }
    for path in task.implicit.iter() {
        fields
            .add("implicit", path_bytes(path))
            .add("content", &digests[path.as_path()]);
    }
    if let Some(closure) = closure {
        fields.add("dependency_closure", closure.as_bytes());
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
    Key(fields.finish())
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

    fn hash(&self) -> blake3::Hash {
        self.0.finalize()
    }

    fn finish(&self) -> String {
        self.hash().to_hex().to_string()
    }
}

/// A path's platform bytes, so distinct non-UTF-8 paths never hash alike.
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_encoded_bytes()
}

/// Owned files matching the target's project globs, plus any workspace file matching its
/// `{workspace}/` globs, plus literal globs naming existing (even gitignored) files.
/// Declared outputs stay inputs: ignored ones are already absent from the file list, and
/// the rest are committed (codegen) or fixed in place. Excludes implicit inputs.
/// Workspace-relative and sorted.
pub fn input_files(ws: &Workspace, project: &Project, target: &Target) -> Result<Vec<PathBuf>> {
    Plan::new(ws).own_inputs(project, &target.inputs)
}

/// What one task's key covers, resolved before any file is hashed.
struct TaskInputs<'a> {
    id: &'a TaskId,
    project: &'a Project,
    target: &'a Target,
    own: Vec<PathBuf>,
    implicit: Rc<[PathBuf]>,
    /// The dependency closure, unless `deps_usage = none`.
    closure: Option<Rc<[&'a str]>>,
}

/// Workspace files every Cargo project's tasks depend on, whatever their `inputs`.
const CARGO_WORKSPACE_FILES: [&str; 10] = [
    "Cargo.lock",
    "Cargo.toml",
    ".cargo/config.toml",
    ".cargo/config",
    "rust-toolchain.toml",
    "rust-toolchain",
    "rustfmt.toml",
    ".rustfmt.toml",
    "clippy.toml",
    ".clippy.toml",
];
/// Workspace files every JS project's tasks depend on, whatever their `inputs`, besides
/// the root tsconfig chain and root files starting with [`JS_CONFIG_PREFIXES`].
const JS_WORKSPACE_FILES: [&str; 6] = [
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "package.json",
    ".npmrc",
    "biome.json",
    "biome.jsonc",
];
const JS_CONFIG_PREFIXES: [&str; 4] = [
    "eslint.config.",
    ".eslintrc",
    ".prettierrc",
    "prettier.config.",
];

/// `rel` names something other than a directory, gitignored or not.
fn exists(root: &Path, rel: &Path) -> bool {
    fs::symlink_metadata(root.join(rel)).is_ok_and(|meta| !meta.is_dir())
}

/// Workspace files matching the `{workspace}/` globs, plus literal ones naming existing
/// files even if gitignored. Sorted.
fn workspace_matches(ws: &Workspace, patterns: &Patterns) -> Vec<PathBuf> {
    if !patterns.has_workspace_globs() {
        return Vec::new();
    }
    let literal = patterns
        .workspace_literals()
        .filter_map(|glob| files::normalize(Path::new(glob)))
        .filter(|path| exists(&ws.root, path));
    ws.files
        .iter()
        .filter(|f| patterns.matches_workspace(f))
        .cloned()
        .chain(literal)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Implicit inputs shared across projects, resolved once per run.
struct WorkspaceInputs {
    cargo: Vec<PathBuf>,
    js: Vec<PathBuf>,
    /// `[workspace] inputs`, hashed by every task.
    every: Vec<PathBuf>,
}

impl WorkspaceInputs {
    fn resolve(ws: &Workspace) -> Result<WorkspaceInputs> {
        let js_configs = ws.files.iter().filter(|f| {
            f.parent().is_some_and(|dir| dir.as_os_str().is_empty())
                && f.to_str()
                    .is_some_and(|name| JS_CONFIG_PREFIXES.iter().any(|p| name.starts_with(p)))
        });
        let globs: Vec<String> = ws
            .config
            .workspace
            .inputs
            .iter()
            .map(|glob| format!("{}{glob}", files::WORKSPACE_PREFIX))
            .collect();
        Ok(WorkspaceInputs {
            cargo: CARGO_WORKSPACE_FILES.iter().map(PathBuf::from).collect(),
            js: JS_WORKSPACE_FILES
                .iter()
                .map(PathBuf::from)
                .chain(ts::config_files(&ws.root))
                .chain(js_configs.cloned())
                .collect(),
            every: workspace_matches(ws, &Patterns::new(&globs)?),
        })
    }
}

/// Per-run memos of everything tasks share: glob sets, the files matching an inputs list
/// in a project, workspace matches, implicit inputs and
/// dependency closures.
struct Plan<'a> {
    ws: &'a Workspace,
    patterns: HashMap<&'a [String], Rc<Patterns>>,
    owned: HashMap<(&'a str, &'a [String]), Rc<[PathBuf]>>,
    shared: HashMap<&'a [String], Rc<[PathBuf]>>,
    implicit: HashMap<&'a str, Rc<[PathBuf]>>,
    workspace_inputs: Option<Rc<WorkspaceInputs>>,
    closures: HashMap<&'a str, Rc<[&'a str]>>,
    /// `(project, inputs)` pairs whose closure files are already in `owned`.
    prepared: HashSet<(&'a str, &'a [String])>,
}

fn memo<K: Eq + Hash, V: Clone>(
    map: &mut HashMap<K, V>,
    key: K,
    make: impl FnOnce() -> Result<V>,
) -> Result<V> {
    match map.entry(key) {
        hash_map::Entry::Occupied(e) => Ok(e.get().clone()),
        hash_map::Entry::Vacant(e) => Ok(e.insert(make()?).clone()),
    }
}

impl<'a> Plan<'a> {
    fn new(ws: &'a Workspace) -> Self {
        Plan {
            ws,
            patterns: HashMap::new(),
            owned: HashMap::new(),
            shared: HashMap::new(),
            implicit: HashMap::new(),
            workspace_inputs: None,
            closures: HashMap::new(),
            prepared: HashSet::new(),
        }
    }

    /// Precomputes the dependency closures of `projects` in parallel.
    fn with_closures(ws: &'a Workspace, projects: HashSet<&'a str>) -> Self {
        let closures: Vec<(&str, Vec<&str>)> = projects
            .into_par_iter()
            .map(|name| (name, ws.dependency_closure(name).into_iter().collect()))
            .collect();
        Plan {
            closures: closures
                .into_iter()
                .map(|(name, closure)| (name, Rc::from(closure)))
                .collect(),
            ..Plan::new(ws)
        }
    }

    fn patterns(&mut self, inputs: &'a [String]) -> Result<Rc<Patterns>> {
        memo(&mut self.patterns, inputs, || {
            Patterns::new(inputs).map(Rc::new)
        })
    }

    /// Files `project` owns that match the project-relative globs, plus literal globs
    /// naming existing files even if gitignored. Only owned files, so a project at the
    /// workspace root never sees its nested projects' files.
    fn owned(&mut self, project: &'a Project, inputs: &'a [String]) -> Result<Rc<[PathBuf]>> {
        let patterns = self.patterns(inputs)?;
        let ws = self.ws;
        memo(&mut self.owned, (project.name.as_str(), inputs), || {
            let root = &project.root;
            let globbed = project
                .files
                .iter()
                .filter(|f| {
                    f.strip_prefix(root)
                        .is_ok_and(|rel| patterns.matches_project(rel))
                })
                .cloned();
            let literal = patterns
                .project_literals()
                .filter_map(|glob| files::normalize(&root.join(glob)))
                .filter(|path| exists(&ws.root, path));
            Ok(globbed
                .chain(literal)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect())
        })
    }

    /// Workspace files matching the `{workspace}/` globs, plus literal ones naming
    /// existing files even if gitignored.
    fn shared(&mut self, inputs: &'a [String]) -> Result<Rc<[PathBuf]>> {
        let patterns = self.patterns(inputs)?;
        let ws = self.ws;
        memo(&mut self.shared, inputs, || {
            Ok(workspace_matches(ws, &patterns).into())
        })
    }

    fn workspace_inputs(&mut self) -> Result<Rc<WorkspaceInputs>> {
        match &self.workspace_inputs {
            Some(inputs) => Ok(inputs.clone()),
            None => Ok(self
                .workspace_inputs
                .insert(Rc::new(WorkspaceInputs::resolve(self.ws)?))
                .clone()),
        }
    }

    /// Lockfiles, workspace manifests, toolchain files and tool configs for the project's
    /// kinds (all of them for explicit projects, which may run anything), `[workspace]
    /// inputs`, and the project's own manifests. No
    /// `inputs` list can drop these; missing ones hash as `missing`.
    fn implicit(&mut self, project: &'a Project) -> Result<Rc<[PathBuf]>> {
        let workspace = self.workspace_inputs()?;
        memo(&mut self.implicit, project.name.as_str(), || {
            Ok({
                let kinds = &project.kinds;
                let uses = |kind: &Kind| kinds.contains(kind) || kinds.contains(&Kind::Explicit);
                let ecosystems = [(Kind::Cargo, &workspace.cargo), (Kind::Js, &workspace.js)]
                    .into_iter()
                    .filter(|(kind, _)| uses(kind))
                    .flat_map(|(_, files)| files.iter().cloned());
                let manifests = [(Kind::Js, "package.json"), (Kind::Cargo, "Cargo.toml")]
                    .into_iter()
                    .map(|(kind, name)| (kind, project.root.join(name)))
                    .filter(|(kind, path)| {
                        kinds.contains(kind) || project.files.binary_search(path).is_ok()
                    })
                    .map(|(_, path)| path);
                ecosystems
                    .chain(workspace.every.iter().cloned())
                    .chain(manifests)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            })
        })
    }

    fn closure(&mut self, name: &'a str) -> Rc<[&'a str]> {
        let ws = self.ws;
        self.closures
            .entry(name)
            .or_insert_with(|| ws.dependency_closure(name).into_iter().collect())
            .clone()
    }

    fn own_inputs(&mut self, project: &'a Project, inputs: &'a [String]) -> Result<Vec<PathBuf>> {
        let owned = self.owned(project, inputs)?;
        let shared = self.shared(inputs)?;
        Ok(owned
            .iter()
            .chain(shared.iter())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect())
    }

    fn task(&mut self, id: &'a TaskId) -> Result<TaskInputs<'a>> {
        let ws = self.ws;
        let project = &ws.projects[&id.project];
        let target = ws.target(id);
        let closure = (target.deps_usage != DepsUsage::None).then(|| self.closure(&project.name));
        let unprepared = closure.as_ref().filter(|_| {
            self.prepared
                .insert((project.name.as_str(), &target.inputs))
        });
        unprepared
            .iter()
            .flat_map(|names| names.iter())
            .try_for_each(|dep| {
                let dep = &ws.projects[*dep];
                self.implicit(dep)?;
                self.owned(dep, &target.inputs).map(drop)
            })?;
        Ok(TaskInputs {
            id,
            project,
            target,
            own: self.own_inputs(project, &target.inputs)?,
            implicit: self.implicit(project)?,
            closure,
        })
    }

    /// Every path some task hashes, each once.
    fn files(&self) -> Vec<&Path> {
        self.owned
            .values()
            .chain(self.shared.values())
            .chain(self.implicit.values())
            .flat_map(|files| files.iter())
            .map(PathBuf::as_path)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    /// The files of `dep` matching `inputs` and its implicit inputs, as one digest shared
    /// by all dependents.
    fn dep_digest(
        &self,
        dep: &str,
        inputs: &[String],
        digests: &HashMap<&Path, String>,
    ) -> blake3::Hash {
        let mut fields = Fields::new();
        let owned = self.owned[&(dep, inputs)].iter().map(|p| ("dep_input", p));
        let implicit = self.implicit[dep].iter().map(|p| ("dep_implicit", p));
        owned.chain(implicit).for_each(|(name, path)| {
            fields
                .add(name, path_bytes(path))
                .add("content", &digests[path.as_path()]);
        });
        fields.hash()
    }
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
    fn only_entries_for_vanished_paths_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["a.txt", "b.txt"] {
            write(dir.path(), f, f);
            back_date(&dir.path().join(f));
        }
        let mut cache = FileHashCache::default();
        cache.hash(dir.path(), Path::new("a.txt")).unwrap();
        cache.hash(dir.path(), Path::new("b.txt")).unwrap();
        cache.save(dir.path()).unwrap();
        let entries = || {
            FileHashCache::load(dir.path())
                .entries
                .into_keys()
                .collect::<Vec<_>>()
        };

        let mut filtered = FileHashCache::load(dir.path());
        filtered.hash(dir.path(), Path::new("a.txt")).unwrap();
        filtered.save(dir.path()).unwrap();
        assert_eq!(entries(), ["a.txt", "b.txt"], "a filtered run keeps b");

        fs::remove_file(dir.path().join("b.txt")).unwrap();
        let mut after_delete = FileHashCache::load(dir.path());
        after_delete.hash(dir.path(), Path::new("a.txt")).unwrap();
        after_delete.save(dir.path()).unwrap();
        assert_eq!(entries(), ["a.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_smallest_failing_path_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "axonal.toml",
            "[projects.\"libs/a\".targets.build]\ncommand = \"make\"\n",
        );
        let names: Vec<String> = (0..16).map(|i| format!("libs/a/f{i:02}.txt")).collect();
        names.iter().for_each(|f| write(dir.path(), f, f));
        let ws = Workspace::discover(dir.path()).unwrap();
        names.iter().rev().for_each(|f| {
            fs::set_permissions(dir.path().join(f), fs::Permissions::from_mode(0o000)).unwrap()
        });
        if fs::read(dir.path().join(&names[0])).is_ok() {
            return; // running as root
        }
        let graph = TaskGraph::build(&ws, &["build".into()], None).unwrap();
        for _ in 0..5 {
            let err = task_keys(
                &ws,
                &graph,
                &mut FileHashCache::default(),
                &Toolchain::default(),
                &no_env,
            )
            .unwrap_err();
            assert!(err.to_string().contains("libs/a/f00.txt"), "{err}");
        }
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

    #[test]
    fn committed_outputs_are_still_inputs() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "axonal.toml",
            r#"
[projects."libs/a".targets.build]
command = "tsc"

[projects."libs/a".targets.codegen]
command = "gen"
outputs = ["src/gen/**"]

[projects."libs/a".targets."lint-fix"]
command = "eslint --fix ."
outputs = ["src/**"]

[projects."apps/b"]
deps = ["libs/a"]

[projects."apps/b".targets.build]
command = "tsc"
"#,
        );
        write(dir.path(), "libs/a/src/main.ts", "1");
        write(dir.path(), "libs/a/src/gen/api.ts", "1");
        write(dir.path(), "apps/b/src/b.ts", "b");
        let keys = || {
            ["build", "lint-fix"]
                .map(|t| key_of(dir.path(), t, "libs/a"))
                .into_iter()
                .chain([key_of(dir.path(), "build", "apps/b")])
                .collect::<Vec<_>>()
        };
        let k0 = keys();
        write(dir.path(), "libs/a/src/gen/api.ts", "hand edit");
        let k1 = keys();
        write(dir.path(), "libs/a/src/main.ts", "2");
        let k2 = keys();
        for i in 0..3 {
            assert_ne!(k0[i], k1[i], "editing src/gen/api.ts changes key {i}");
            assert_ne!(k1[i], k2[i], "editing src/main.ts changes key {i}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn files_inside_symlinked_directories_change_keys() {
        use std::os::unix::fs::symlink;
        let dir = one_project();
        write(dir.path(), "shared/util.ts", "export const x = 1");
        write(dir.path(), "shared/node_modules/dep/index.js", "1");
        symlink(".", dir.path().join("shared/loop")).unwrap();
        symlink("../../../shared", dir.path().join("libs/a/src/shared")).unwrap();
        let before = key_of(dir.path(), "build", "libs/a");
        write(dir.path(), "shared/node_modules/dep/index.js", "2");
        assert_eq!(
            key_of(dir.path(), "build", "libs/a"),
            before,
            "node_modules is skipped"
        );
        write(dir.path(), "shared/util.ts", "export const x = 2");
        let edited = key_of(dir.path(), "build", "libs/a");
        assert_ne!(edited, before);
        write(dir.path(), "shared/nested/new.ts", "");
        assert_ne!(key_of(dir.path(), "build", "libs/a"), edited);
    }

    /// Runs `f` on a thread; true if it hasn't finished after 1.5 s, in which case
    /// `unblock` runs so `f` can finish.
    #[cfg(unix)]
    fn hangs(f: impl FnOnce() + Send + 'static, unblock: impl FnOnce()) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            f();
            let _ = tx.send(());
        });
        let hung = rx.recv_timeout(Duration::from_millis(1500)).is_err();
        if hung {
            unblock();
        }
        hung
    }

    #[cfg(unix)]
    #[test]
    fn fifo_inputs_hash_as_special_without_opening() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write(
            &root,
            "axonal.toml",
            "[projects.\"libs/a\".targets.build]\ncommand = \"m\"\ninputs = [\"src/**\", \"pipe\", \"via-link\"]\n",
        );
        write(&root, "libs/a/src/a.txt", "a");
        let fifo = root.join("libs/a/pipe");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        std::os::unix::fs::symlink("pipe", root.join("libs/a/via-link")).unwrap();
        let r = root.clone();
        let hung = hangs(
            move || {
                key_of(&r, "build", "libs/a");
            },
            || {
                let _ = fs::OpenOptions::new().write(true).open(&fifo);
            },
        );
        assert!(!hung, "hashing a FIFO must not block");
        let mut cache = FileHashCache::default();
        assert_eq!(
            cache.hash(&root, Path::new("libs/a/pipe")).unwrap(),
            SPECIAL
        );
        assert_ne!(
            cache.hash(&root, Path::new("libs/a/via-link")).unwrap(),
            MISSING
        );
    }

    fn all_keys(ws: &Workspace, graph: &TaskGraph) -> BTreeMap<TaskId, Key> {
        task_keys(
            ws,
            graph,
            &mut FileHashCache::default(),
            &Toolchain::default(),
            &no_env,
        )
        .unwrap()
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
    }

    const TARGETS: [&str; 4] = ["build", "test", "lint", "fmt"];

    /// `n` explicit projects with random deps and targets, outputs, gitignored literal
    /// inputs and a `{workspace}/` input.
    fn random_workspace(n: usize, seed: u64) -> tempfile::TempDir {
        use std::fmt::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let mut rng = Lcg(seed);
        let mut config = String::from(
            r#"
[targets.build]
depends_on = ["^build"]
outputs = ["dist/**"]

[targets.test]
inputs = ["src/**", "test/**", ".env.local"]

[targets.lint]
inputs = ["src/**", "{workspace}/eslint.cfg"]

[targets.fmt]
inputs = ["src/**"]
"#,
        );
        for i in 0..n {
            let deps: Vec<String> = (0..i)
                .filter(|_| rng.next().is_multiple_of(5))
                .map(|d| format!("\"p{d:02}\""))
                .collect();
            writeln!(config, "[projects.p{i:02}]\ndeps = [{}]\n", deps.join(", ")).unwrap();
            for t in TARGETS {
                if t == "build" || !rng.next().is_multiple_of(3) {
                    writeln!(
                        config,
                        "[projects.p{i:02}.targets.{t}]\ncommand = \"{t} {i}\"\n"
                    )
                    .unwrap();
                }
            }
            write(dir.path(), &format!("p{i:02}/src/m.ts"), &format!("{i}"));
            if rng.next().is_multiple_of(2) {
                write(dir.path(), &format!("p{i:02}/test/t.ts"), "t");
            }
            write(dir.path(), &format!("p{i:02}/README.md"), "r");
            write(dir.path(), &format!("p{i:02}/dist/out.js"), "o");
            if rng.next().is_multiple_of(4) {
                write(dir.path(), &format!("p{i:02}/.env.local"), "e");
            }
        }
        write(dir.path(), ".gitignore", ".env.local\n");
        write(dir.path(), "eslint.cfg", "rules");
        write(dir.path(), "pnpm-lock.yaml", "l");
        write(dir.path(), "axonal.toml", &config);
        dir
    }

    fn every_target(ws: &Workspace) -> TaskGraph {
        TaskGraph::build(ws, &TARGETS.map(String::from), None).unwrap()
    }

    #[test]
    fn memo_single_root_graphs_match_full_graph() {
        for seed in 1..=5 {
            let dir = random_workspace(30, seed);
            let ws = Workspace::discover(dir.path()).unwrap();
            let graph = every_target(&ws);
            let full = all_keys(&ws, &graph);
            for id in &graph.order {
                let single = TaskGraph::from_roots(&ws, vec![id.clone()]).unwrap();
                assert_eq!(all_keys(&ws, &single)[id], full[id], "seed {seed} {id}");
            }
        }
    }

    /// The files a task's key should cover, computed per task with fresh memos.
    fn reference_files(ws: &Workspace, id: &TaskId) -> BTreeSet<PathBuf> {
        let project = &ws.projects[&id.project];
        let target = ws.target(id);
        let mut plan = Plan::new(ws);
        let own = plan.own_inputs(project, &target.inputs).unwrap();
        let implicit = plan.implicit(project).unwrap();
        let deps = (target.deps_usage != DepsUsage::None)
            .then(|| ws.dependency_closure(&project.name))
            .into_iter()
            .flatten()
            .flat_map(|dep| {
                let dep = &ws.projects[dep];
                let mut fresh = Plan::new(ws);
                let owned = fresh.owned(dep, &target.inputs).unwrap();
                let implicit = fresh.implicit(dep).unwrap();
                owned
                    .iter()
                    .chain(implicit.iter())
                    .cloned()
                    .collect::<Vec<_>>()
            });
        own.into_iter()
            .chain(implicit.iter().cloned())
            .chain(deps)
            .collect()
    }

    #[test]
    fn memo_file_edits_change_exactly_the_expected_keys_seed_7() {
        file_edits_change_exactly_the_expected_keys(7);
    }

    #[test]
    fn memo_file_edits_change_exactly_the_expected_keys_seed_8() {
        file_edits_change_exactly_the_expected_keys(8);
    }

    fn file_edits_change_exactly_the_expected_keys(seed: u64) {
        let dir = random_workspace(25, seed);
        let ws = Workspace::discover(dir.path()).unwrap();
        let graph = every_target(&ws);
        let base = all_keys(&ws, &graph);
        let refs: BTreeMap<&TaskId, BTreeSet<PathBuf>> = graph
            .order
            .iter()
            .map(|id| (id, reference_files(&ws, id)))
            .collect();
        let candidates: BTreeSet<PathBuf> = ws
            .files
            .iter()
            .cloned()
            .chain(refs.values().flatten().cloned())
            .filter(|f| dir.path().join(f).is_file())
            .collect();
        for file in &candidates {
            let abs = dir.path().join(file);
            let original = fs::read(&abs).unwrap();
            fs::write(&abs, [original.as_slice(), b"!"].concat()).unwrap();
            let edited = all_keys(&ws, &graph);
            fs::write(&abs, &original).unwrap();
            let expected = graph.order.iter().fold(BTreeSet::new(), |mut hit, id| {
                if refs[id].contains(file) || graph.deps[id].iter().any(|d| hit.contains(d)) {
                    hit.insert(id);
                }
                hit
            });
            let changed: BTreeSet<&TaskId> = graph
                .order
                .iter()
                .filter(|id| base[*id] != edited[*id])
                .collect();
            assert_eq!(changed, expected, "seed {seed} file {}", file.display());
        }
        assert!(candidates.len() > 50, "{}", candidates.len());
        assert_eq!(all_keys(&ws, &graph), base);
    }

    #[test]
    fn keys_do_not_depend_on_the_thread_count() {
        let dir = random_workspace(40, 11);
        let ws = Workspace::discover(dir.path()).unwrap();
        let graph = every_target(&ws);
        let with = |threads: usize| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| all_keys(&ws, &graph))
        };
        let one = with(1);
        for threads in [2, 8, 16] {
            for _ in 0..5 {
                assert_eq!(with(threads), one, "{threads} threads");
            }
        }
    }

    #[test]
    fn gitignored_literal_inputs_are_hashed() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "axonal.toml",
            "[projects.\"libs/a\".targets.build]\ncommand = \"make\"\ninputs = [\"src/**\", \".env.local\", \"{workspace}/.secrets\"]\n",
        );
        write(dir.path(), ".gitignore", ".env.local\n.secrets\n");
        write(dir.path(), "libs/a/src/a.txt", "a");
        write(dir.path(), "libs/a/.env.local", "API=1");
        write(dir.path(), ".secrets", "1");
        let before = key_of(dir.path(), "build", "libs/a");
        write(dir.path(), "libs/a/.env.local", "API=2");
        let env_changed = key_of(dir.path(), "build", "libs/a");
        assert_ne!(before, env_changed);
        write(dir.path(), ".secrets", "2");
        assert_ne!(key_of(dir.path(), "build", "libs/a"), env_changed);
    }

    #[test]
    fn explicit_projects_see_every_lockfile() {
        let dir = one_project();
        write(dir.path(), "pnpm-lock.yaml", "lodash: 4.17.20");
        write(dir.path(), "Cargo.lock", "serde 1.0.1");
        let before = key_of(dir.path(), "build", "libs/a");
        write(dir.path(), "pnpm-lock.yaml", "lodash: 4.17.21");
        let pnpm = key_of(dir.path(), "build", "libs/a");
        assert_ne!(before, pnpm);
        write(dir.path(), "Cargo.lock", "serde 1.0.2");
        assert_ne!(key_of(dir.path(), "build", "libs/a"), pnpm);
    }

    /// A Cargo member `c` and a pnpm member `p`, which overrides its inputs.
    fn mixed_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/c\"]\nresolver = \"2\"\n",
        );
        write(
            dir.path(),
            "crates/c/Cargo.toml",
            "[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(dir.path(), "crates/c/src/lib.rs", "");
        write(dir.path(), "Cargo.lock", "version = 3\n");
        write(
            dir.path(),
            "pnpm-workspace.yaml",
            "packages:\n  - 'packages/*'\n",
        );
        write(dir.path(), "package.json", r#"{"private":true}"#);
        write(
            dir.path(),
            "packages/p/package.json",
            r#"{"name":"p","scripts":{"build":"tsc"}}"#,
        );
        write(dir.path(), "packages/p/src/index.ts", "");
        write(dir.path(), "pnpm-lock.yaml", "lockfileVersion: '9.0'\n");
        write(
            dir.path(),
            "axonal.toml",
            "[projects.\"packages/p\".targets.build]\ninputs = [\"src/**\"]\n",
        );
        dir
    }

    #[test]
    fn workspace_manifests_and_lockfiles_change_member_keys() {
        let dir = mixed_workspace();
        let both = || {
            (
                key_of(dir.path(), "build", "c"),
                key_of(dir.path(), "build", "p"),
            )
        };
        let (c, p) = both();
        write(dir.path(), "Cargo.lock", "version = 3\n# bumped\n");
        let (c2, p2) = both();
        assert_ne!(c, c2, "Cargo.lock is a Cargo input");
        assert_eq!(p, p2, "Cargo.lock is not a pnpm input");
        write(dir.path(), "pnpm-lock.yaml", "lockfileVersion: '9.1'\n");
        let (c3, p3) = both();
        assert_eq!(c2, c3);
        assert_ne!(p2, p3, "pnpm-lock.yaml is a pnpm input");
        write(
            dir.path(),
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"1.96\"\n",
        );
        write(dir.path(), ".npmrc", "strict-peer-dependencies=true\n");
        let (c4, p4) = both();
        assert_ne!(c3, c4);
        assert_ne!(p3, p4);
    }

    #[test]
    fn custom_inputs_still_see_their_own_manifest() {
        let dir = mixed_workspace();
        let before = key_of(dir.path(), "build", "p");
        write(
            dir.path(),
            "packages/p/package.json",
            r#"{"name":"p","version":"2.0.0","scripts":{"build":"tsc"}}"#,
        );
        assert_ne!(key_of(dir.path(), "build", "p"), before);
    }

    #[test]
    fn root_js_tool_configs_change_pnpm_member_keys() {
        let dir = mixed_workspace();
        write(
            dir.path(),
            "tsconfig.json",
            r#"{"extends":"./configs/base"}"#,
        );
        write(
            dir.path(),
            "configs/base.json",
            r#"{"compilerOptions":{"strict":false}}"#,
        );
        let edits = [
            (
                "configs/base.json",
                r#"{"compilerOptions":{"strict":true}}"#,
            ),
            ("tsconfig.base.json", "{}"),
            ("eslint.config.js", "export default []"),
            (".eslintrc.json", "{}"),
            (".prettierrc", "{}"),
            ("prettier.config.mjs", "export default {}"),
            ("biome.json", "{}"),
            ("biome.jsonc", "{}"),
        ];
        let c = key_of(dir.path(), "build", "c");
        edits
            .iter()
            .fold(key_of(dir.path(), "build", "p"), |before, (file, body)| {
                write(dir.path(), file, body);
                let after = key_of(dir.path(), "build", "p");
                assert_ne!(before, after, "{file}");
                after
            });
        assert_eq!(
            key_of(dir.path(), "build", "c"),
            c,
            "JS configs aren't Cargo inputs"
        );
    }

    #[test]
    fn root_rust_tool_configs_change_cargo_member_keys() {
        let dir = mixed_workspace();
        let p = key_of(dir.path(), "build", "p");
        [
            "rustfmt.toml",
            ".rustfmt.toml",
            "clippy.toml",
            ".clippy.toml",
        ]
        .iter()
        .fold(key_of(dir.path(), "build", "c"), |before, file| {
            write(dir.path(), file, "max_width = 80\n");
            let after = key_of(dir.path(), "build", "c");
            assert_ne!(before, after, "{file}");
            after
        });
        assert_eq!(
            key_of(dir.path(), "build", "p"),
            p,
            "Rust configs aren't JS inputs"
        );
    }

    #[test]
    fn configured_workspace_inputs_change_every_key() {
        let dir = fixture();
        write(
            dir.path(),
            "axonal.toml",
            &format!("[workspace]\ninputs = [\".tool-versions\", \"ci/*.env\"]\n{CONFIG}"),
        );
        write(dir.path(), ".gitignore", ".tool-versions\n");
        write(dir.path(), ".tool-versions", "node 24");
        write(dir.path(), "ci/prod.env", "A=1");
        let every = || {
            [("build", "libs/a"), ("build", "apps/b"), ("fmt", "apps/b")]
                .map(|(target, project)| key_of(dir.path(), target, project))
        };
        let k0 = every();
        write(dir.path(), ".tool-versions", "node 26");
        let k1 = every();
        write(dir.path(), "ci/prod.env", "A=2");
        let k2 = every();
        for i in 0..3 {
            assert_ne!(k0[i], k1[i], "gitignored literal, key {i}");
            assert_ne!(k1[i], k2[i], "glob, key {i}");
        }
    }

    #[test]
    fn missing_tools_probe_as_missing() {
        let probes = [Probe {
            name: "nope",
            program: "axonal-no-such-tool",
            args: &[],
        }];
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            run_probes(dir.path(), &probes, Duration::from_secs(5))["nope"],
            "missing"
        );
    }

    #[cfg(unix)]
    #[test]
    fn probes_run_in_the_workspace_root() {
        let probes = [Probe {
            name: "cwd",
            program: "sh",
            args: &["-c", "pwd -P"],
        }];
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            run_probes(dir.path(), &probes, Duration::from_secs(5))["cwd"],
            dir.path().canonicalize().unwrap().to_str().unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn slow_probes_time_out_and_run_in_parallel() {
        let probes = [
            Probe {
                name: "slow",
                program: "sleep",
                args: &["30"],
            },
            Probe {
                name: "also_slow",
                program: "sleep",
                args: &["30"],
            },
        ];
        let dir = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let versions = run_probes(dir.path(), &probes, Duration::from_millis(300));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(versions["slow"], "timeout");
        assert_eq!(versions["also_slow"], "timeout");
    }

    /// `cargo test --release hash::tests::scale -- --ignored --nocapture`; set
    /// `AXONAL_SCALE_N` to change the project count.
    #[test]
    #[ignore]
    fn scale() {
        use std::{fmt::Write as _, time::Instant};
        let n: usize = std::env::var("AXONAL_SCALE_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2000);
        let dir = tempfile::tempdir().unwrap();
        let mut config =
            String::from("[targets.build]\ndepends_on = [\"^build\"]\n\n[targets.test]\n\n");
        for i in 0..n {
            let deps = if i == 0 {
                String::new()
            } else {
                format!("\"p/{:04}\"", i - 1)
            };
            writeln!(config, "[projects.\"p/{i:04}\"]\ndeps = [{deps}]\n").unwrap();
            writeln!(
                config,
                "[projects.\"p/{i:04}\".targets.build]\ncommand = \"b\"\n"
            )
            .unwrap();
            writeln!(
                config,
                "[projects.\"p/{i:04}\".targets.test]\ncommand = \"t\"\n"
            )
            .unwrap();
            for f in 0..20 {
                let rel = format!("p/{i:04}/src/f{f}.ts");
                write(dir.path(), &rel, &format!("{i}-{f}"));
                back_date(&dir.path().join(rel));
            }
        }
        write(dir.path(), "axonal.toml", &config);
        let ws = Workspace::discover(dir.path()).unwrap();
        let graph = TaskGraph::build(&ws, &["build".into(), "test".into()], None).unwrap();
        let run = |label: &str| {
            let mut cache = FileHashCache::load(dir.path());
            let start = Instant::now();
            let keys = task_keys(&ws, &graph, &mut cache, &Toolchain::default(), &no_env).unwrap();
            println!("{label}: {} keys in {:?}", keys.len(), start.elapsed());
            cache.save(dir.path()).unwrap();
        };
        run("cold");
        run("warm");
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
