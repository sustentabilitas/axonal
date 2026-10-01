//! Runs a task graph: dependency order, bounded parallelism, cache restore and save,
//! and output prefixed with `project:target`.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    future::pending,
    io::{self, Write},
    path::Path,
    pin::pin,
    process::Stdio,
    sync::{Arc, Mutex, PoisonError},
    time::Instant,
};

use serde::Serialize;
use tap::Tap;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::{Child, Command},
    task::{self, JoinSet},
};

use crate::{
    cache::{CacheError, LogLine, Meta, Store, archive},
    files::Patterns,
    graph::{TaskGraph, TaskId, Workspace},
    hash::Key,
};

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub parallel: usize,
    pub keep_going: bool,
    pub use_cache: bool,
    /// Send task output to stderr so stdout carries only the JSON report.
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ran,
    CacheHit,
    /// Exited non-zero, couldn't start, was interrupted, or its runner panicked.
    Failed,
    /// Not run because a dependency failed, or fail-fast or an interrupt stopped scheduling.
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskResult {
    pub task: TaskId,
    pub outcome: Outcome,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub key: Key,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RunReport {
    pub tasks: Vec<TaskResult>,
    pub warnings: Vec<String>,
}

impl RunReport {
    pub fn count(&self, outcome: Outcome) -> usize {
        self.tasks.iter().filter(|t| t.outcome == outcome).count()
    }

    pub fn exit_code(&self) -> u8 {
        u8::from(self.count(Outcome::Failed) + self.count(Outcome::Skipped) > 0)
    }

    pub fn summary(&self) -> String {
        format!(
            "{} tasks: {} ran, {} cache hits, {} failed, {} skipped",
            self.tasks.len(),
            self.count(Outcome::Ran),
            self.count(Outcome::CacheHit),
            self.count(Outcome::Failed),
            self.count(Outcome::Skipped),
        )
    }
}

pub fn default_parallelism() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// Runs every task in `graph` once its dependencies have succeeded, at most `opts.parallel`
/// at a time.
///
/// Persistent tasks are never cached and take no slot, so servers that never exit can't
/// starve the tasks around them; the run ends only once they exit.
///
/// Ctrl-C (and SIGTERM on Unix) interrupts the run: running tasks are killed and `Failed`,
/// the rest `Skipped`. Dropping the future kills running tasks too. On Unix, killing a task
/// kills its whole process group, so the processes it started go with it.
///
/// # Panics
///
/// Panics if `keys` lacks a task of `graph`.
pub async fn run(
    ws: Arc<Workspace>,
    graph: &TaskGraph,
    keys: &BTreeMap<TaskId, Key>,
    store: Arc<dyn Store>,
    opts: &RunOptions,
) -> RunReport {
    let dependents = graph.dependents();
    let mut waiting: BTreeMap<&TaskId, usize> =
        graph.deps.iter().map(|(id, d)| (id, d.len())).collect();
    let mut ready: VecDeque<TaskId> = graph
        .order
        .iter()
        .filter(|id| graph.deps.get(*id).is_none_or(BTreeSet::is_empty))
        .cloned()
        .collect();
    let mut running = JoinSet::new();
    let mut jobs: HashMap<task::Id, Started> = HashMap::new();
    let mut busy = 0;
    let mut results: BTreeMap<TaskId, TaskResult> = BTreeMap::new();
    let mut warnings = Vec::new();
    let mut stopped = false;
    let mut interrupt = pin!(interrupted());
    loop {
        if !stopped {
            let is_persistent = |id: &TaskId| ws.get_target(id).is_some_and(|t| t.persistent);
            let (persistent, rest): (VecDeque<_>, VecDeque<_>) =
                ready.drain(..).partition(is_persistent);
            ready = rest;
            let free = opts.parallel.max(1).saturating_sub(busy).min(ready.len());
            for id in persistent.into_iter().chain(ready.drain(..free)) {
                let persistent = is_persistent(&id);
                busy += usize::from(!persistent);
                let job = Job {
                    ws: ws.clone(),
                    key: keys[&id].clone(),
                    id: id.clone(),
                    store: store.clone(),
                    use_cache: opts.use_cache,
                    to_stderr: opts.json,
                };
                let handle = running.spawn(job.execute());
                jobs.insert(
                    handle.id(),
                    Started {
                        task: id,
                        persistent,
                        at: Instant::now(),
                    },
                );
            }
        }
        let joined = tokio::select! {
            joined = running.join_next_with_id() => match joined {
                Some(joined) => joined,
                None => break,
            },
            () = &mut interrupt => {
                warnings.push(format!("interrupted: killed {} running tasks", running.len()));
                running.shutdown().await;
                results.extend(
                    jobs.drain()
                        .map(|(_, s)| (s.task.clone(), s.failed(keys))),
                );
                break;
            }
        };
        let id = match &joined {
            Ok((id, _)) => *id,
            Err(e) => e.id(),
        };
        let Some(started) = jobs.remove(&id) else {
            continue;
        };
        busy -= usize::from(!started.persistent);
        let (result, mut job_warnings) = joined
            .map(|(_, done)| done)
            .unwrap_or_else(|e| (started.failed(keys), vec![format!("{}: {e}", started.task)]));
        warnings.append(&mut job_warnings);
        if result.outcome == Outcome::Failed {
            stopped |= !opts.keep_going;
        } else {
            for dependent in dependents.get(&result.task).into_iter().flatten() {
                if let Some(n) = waiting.get_mut(dependent) {
                    *n -= 1;
                    if *n == 0 {
                        ready.push_back(dependent.clone());
                    }
                }
            }
        }
        results.insert(result.task.clone(), result);
    }
    let tasks = graph
        .order
        .iter()
        .map(|id| {
            results.remove(id).unwrap_or_else(|| TaskResult {
                task: id.clone(),
                outcome: Outcome::Skipped,
                exit_code: None,
                duration_ms: 0,
                key: keys[id].clone(),
            })
        })
        .collect();
    RunReport { tasks, warnings }
}

/// A spawned job, for results it can't report itself.
struct Started {
    task: TaskId,
    persistent: bool,
    at: Instant,
}

impl Started {
    fn failed(&self, keys: &BTreeMap<TaskId, Key>) -> TaskResult {
        TaskResult {
            task: self.task.clone(),
            outcome: Outcome::Failed,
            exit_code: None,
            duration_ms: elapsed_ms(self.at),
            key: keys[&self.task].clone(),
        }
    }
}

/// Resolves on Ctrl-C, or SIGINT or SIGTERM on Unix; never if no handler could be
/// installed. Handlers are installed on the call, not the first poll.
fn interrupted() -> impl Future<Output = ()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{Signal, SignalKind, signal};
        async fn received(signal: Option<Signal>) {
            if let Some(mut signal) = signal
                && signal.recv().await.is_some()
            {
                return;
            }
            pending().await
        }
        let int = signal(SignalKind::interrupt()).ok();
        let term = signal(SignalKind::terminate()).ok();
        async move {
            tokio::select! {
                () = received(int) => {}
                () = received(term) => {}
            }
        }
    }
    #[cfg(not(unix))]
    async {
        if tokio::signal::ctrl_c().await.is_err() {
            pending().await
        }
    }
}

struct Job {
    ws: Arc<Workspace>,
    id: TaskId,
    key: Key,
    store: Arc<dyn Store>,
    use_cache: bool,
    to_stderr: bool,
}

impl Job {
    async fn execute(self) -> (TaskResult, Vec<String>) {
        let started = Instant::now();
        let target = self.ws.target(&self.id).clone();
        let cwd = self.ws.root.join(&self.ws.projects[&self.id.project].root);
        let cacheable = self.use_cache && !target.persistent;
        let mut warnings = Vec::new();
        if cacheable {
            match self.restore(&target.outputs).await {
                Ok(Some(logs)) => {
                    logs.iter()
                        .for_each(|line| self.emit(&line.text, line.stderr));
                    return (self.result(Outcome::CacheHit, Some(0), started), warnings);
                }
                Ok(None) => {}
                Err(e) => warnings.push(format!(
                    "{}: cache read failed, running instead: {e:#}",
                    self.id
                )),
            }
        }
        let (code, logs) = match self.spawn(&target.command, &cwd).await {
            Ok(done) => done,
            Err(e) => {
                let message = format!("failed to start `{}`: {e}", target.command);
                self.emit(&message, true);
                (-1, vec![])
            }
        };
        if code == 0 && cacheable {
            let duration_ms = elapsed_ms(started);
            match self.save(&target.outputs, code, duration_ms, logs).await {
                Ok(()) => {}
                Err(e) if e.downcast_ref().is_some_and(is_uncacheable) => {
                    warnings.push(format!("{}: outputs not cached: {e:#}", self.id));
                }
                Err(e) => warnings.push(format!("{}: cache write failed: {e:#}", self.id)),
            }
        }
        let outcome = if code == 0 {
            Outcome::Ran
        } else {
            Outcome::Failed
        };
        (self.result(outcome, Some(code), started), warnings)
    }

    fn result(&self, outcome: Outcome, exit_code: Option<i32>, started: Instant) -> TaskResult {
        TaskResult {
            task: self.id.clone(),
            outcome,
            exit_code,
            duration_ms: elapsed_ms(started),
            key: self.key.clone(),
        }
    }

    /// Replays a cached success, replacing the outputs present. Corrupt entries are removed
    /// by the store, broken archives here; both surface as errors (warned, then run).
    async fn restore(&self, outputs: &[String]) -> anyhow::Result<Option<Vec<LogLine>>> {
        let patterns = Patterns::new(outputs)?;
        let (store, key, root) = (self.store.clone(), self.key.clone(), self.ws.root.clone());
        let project_root = self.ws.projects[&self.id.project].root.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Vec<LogLine>>> {
            let Some(entry) = store.get(&key)? else {
                return Ok(None);
            };
            if entry.meta.exit_code != 0 {
                return Ok(None);
            }
            let stale = patterns.existing_files(&root, &project_root)?;
            match archive::restore(&root, &entry.meta, &entry.archive, &stale) {
                // A broken archive would fail every hit: drop it so the next run re-saves.
                Err(e) if e.is_archive_fault() => {
                    let _ = store.remove(&key);
                    Err(e.into())
                }
                other => other.map(|()| Some(entry.meta.logs)).map_err(Into::into),
            }
        })
        .await?
    }

    async fn save(
        &self,
        outputs: &[String],
        exit_code: i32,
        duration_ms: u64,
        logs: Vec<LogLine>,
    ) -> anyhow::Result<()> {
        let patterns = Patterns::new(outputs)?;
        let (store, key, root) = (self.store.clone(), self.key.clone(), self.ws.root.clone());
        let project_root = self.ws.projects[&self.id.project].root.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let files = patterns.existing_files(&root, &project_root)?;
            let packed = archive::pack(&root, &files, 0)?;
            let meta = Meta::new(key.clone(), exit_code, duration_ms, logs, &packed);
            Ok(store.put(&key, &meta, packed.path())?)
        })
        .await?
    }

    async fn spawn(&self, command: &str, cwd: &Path) -> io::Result<(i32, Vec<LogLine>)> {
        let mut child = shell(command)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let group = Group::of(&child);
        let logs = Mutex::new(Vec::new());
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let (out, err) = tokio::join!(
            self.pump(stdout, &logs, false),
            self.pump(stderr, &logs, true)
        );
        out.and(err)?;
        let status = child.wait().await?;
        group.disarm();
        Ok((
            status.code().unwrap_or(-1),
            logs.into_inner().unwrap_or_else(PoisonError::into_inner),
        ))
    }

    async fn pump(
        &self,
        stream: Option<impl AsyncRead + Unpin>,
        logs: &Mutex<Vec<LogLine>>,
        is_stderr: bool,
    ) -> io::Result<()> {
        let Some(stream) = stream else {
            return Ok(());
        };
        let mut reader = BufReader::new(stream);
        let mut buf = Vec::new();
        while reader.read_until(b'\n', &mut buf).await? > 0 {
            {
                let text = String::from_utf8_lossy(&buf);
                let line = text.trim_end_matches(['\n', '\r']);
                self.emit(line, is_stderr);
                logs.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(LogLine {
                        stderr: is_stderr,
                        text: line.into(),
                    });
            }
            buf.clear();
        }
        Ok(())
    }

    fn emit(&self, line: &str, is_stderr: bool) {
        let text = format!("{} | {line}\n", self.id);
        let _ = if is_stderr || self.to_stderr {
            io::stderr().lock().write_all(text.as_bytes())
        } else {
            io::stdout().lock().write_all(text.as_bytes())
        };
    }
}

/// Outputs that `pack` refuses: the save is skipped, not failed.
fn is_uncacheable(e: &CacheError) -> bool {
    matches!(e, CacheError::Symlink(_) | CacheError::NotAFile(_))
}

/// A task's process group (on Unix), killed with everything in it if dropped armed, e.g.
/// when the run is interrupted or dropped mid-task. Disarmed once the shell, the group
/// leader, is reaped: until then its pid, which is the group id, can't be reused.
#[cfg_attr(not(unix), allow(dead_code))]
struct Group(Option<u32>);

impl Group {
    fn of(child: &Child) -> Group {
        Group(child.id())
    }

    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.0.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
            // SAFETY: killpg only sends a signal, to a group whose leader we haven't reaped.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
}

/// The platform shell, running `command` in a new process group on Unix.
fn shell(command: &str) -> Command {
    let (program, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    Command::new(program).tap_mut(|cmd| {
        cmd.args([flag, command]);
        #[cfg(unix)]
        cmd.process_group(0);
    })
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Local;
    use crate::graph::testing::{project, workspace};

    fn result(task: &str, outcome: Outcome) -> TaskResult {
        TaskResult {
            task: TaskId::new(task, "build"),
            outcome,
            exit_code: None,
            duration_ms: 0,
            key: Key("k".into()),
        }
    }

    #[test]
    fn summary_counts_outcomes_and_failures_exit_1() {
        let report = RunReport {
            tasks: vec![
                result("a", Outcome::Ran),
                result("b", Outcome::CacheHit),
                result("c", Outcome::Failed),
                result("d", Outcome::Skipped),
            ],
            warnings: vec![],
        };
        assert_eq!(
            report.summary(),
            "4 tasks: 1 ran, 1 cache hits, 1 failed, 1 skipped"
        );
        assert_eq!(report.exit_code(), 1);
        assert_eq!(RunReport::default().exit_code(), 0);
    }

    #[tokio::test]
    async fn runs_dependencies_first_then_hits_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        // `Workspace::root` is canonical; a macOS tempdir is not (`/var` → `/private/var`).
        let root = dir.path().canonicalize().unwrap();
        let mut lib = project("lib", &[], &[("build", &[])]);
        lib.targets.get_mut("build").unwrap().command = "echo lib > ../order.txt".into();
        lib.targets.get_mut("build").unwrap().outputs = vec!["{workspace}/order.txt".into()];
        let mut app = project("app", &["lib"], &[("build", &["^build"])]);
        app.targets.get_mut("build").unwrap().command = "cat ../order.txt".into();
        for p in ["lib", "app"] {
            std::fs::create_dir_all(root.join(p)).unwrap();
        }
        let mut ws = workspace(vec![lib, app]);
        ws.root = root.clone();
        let graph = TaskGraph::build(&ws, &["build".into()], None).unwrap();
        let keys: BTreeMap<TaskId, Key> = graph
            .order
            .iter()
            .map(|id| (id.clone(), Key(id.to_string().replace([':', '/'], "_"))))
            .collect();
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        let opts = RunOptions {
            parallel: 2,
            keep_going: false,
            use_cache: true,
            json: true,
        };
        let ws = Arc::new(ws);

        let first = run(ws.clone(), &graph, &keys, store.clone(), &opts).await;
        assert_eq!(
            first.summary(),
            "2 tasks: 2 ran, 0 cache hits, 0 failed, 0 skipped"
        );

        std::fs::remove_file(root.join("order.txt")).unwrap();
        let second = run(ws, &graph, &keys, store, &opts).await;
        assert_eq!(
            second.summary(),
            "2 tasks: 0 ran, 2 cache hits, 0 failed, 0 skipped"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("order.txt")).unwrap(),
            "lib\n"
        );
    }

    fn keys(graph: &TaskGraph) -> BTreeMap<TaskId, Key> {
        graph
            .order
            .iter()
            .map(|id| (id.clone(), Key(id.to_string().replace([':', '/'], "_"))))
            .collect()
    }

    fn options(parallel: usize) -> RunOptions {
        RunOptions {
            parallel,
            keep_going: false,
            use_cache: true,
            json: true,
        }
    }

    /// A workspace at `root` with a directory for each project.
    fn ws_at(root: &Path, projects: Vec<crate::graph::Project>) -> Workspace {
        projects
            .iter()
            .for_each(|p| std::fs::create_dir_all(root.join(&p.name)).unwrap());
        workspace(projects).tap_mut(|ws| ws.root = root.to_path_buf())
    }

    /// A one-project workspace whose `build` runs `command`.
    fn single(root: &Path, command: &str) -> (Arc<Workspace>, TaskGraph) {
        let p = project("p", &[], &[("build", &[])])
            .tap_mut(|p| p.targets.get_mut("build").unwrap().command = command.into());
        let ws = ws_at(root, vec![p]);
        let graph = TaskGraph::build(&ws, &["build".into()], None).unwrap();
        (Arc::new(ws), graph)
    }

    fn stored(store: &Arc<dyn Store>, graph: &TaskGraph) -> Option<Meta> {
        store
            .get(&keys(graph)[&graph.order[0]])
            .unwrap()
            .map(|entry| entry.meta)
    }

    fn line(stderr: bool, text: &str) -> LogLine {
        LogLine {
            stderr,
            text: text.into(),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn logs_are_stored_with_their_streams() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // Lines on different pipes are ordered as read, so the pause makes this one certain.
        let (ws, graph) = single(&root, "echo out; sleep 0.2; echo err >&2");
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        run(ws, &graph, &keys(&graph), store.clone(), &options(1)).await;
        assert_eq!(
            stored(&store, &graph).unwrap().logs,
            [line(false, "out"), line(true, "err")]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_output_path_warns_and_runs_uncached() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut lib = project("lib", &[], &[("build", &[])]);
        let build = lib.targets.get_mut("build").unwrap();
        build.command = "mkdir -p dist && echo x > dist/a.js".into();
        build.outputs = vec!["dist/**".into()];
        std::fs::create_dir_all(root.join("lib")).unwrap();
        let mut ws = workspace(vec![lib]);
        ws.root = root.clone();
        let graph = TaskGraph::build(&ws, &["build".into()], None).unwrap();
        let keys = keys(&graph);
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        let ws = Arc::new(ws);
        let first = run(ws.clone(), &graph, &keys, store.clone(), &options(1)).await;
        assert_eq!(
            first.summary(),
            "1 tasks: 1 ran, 0 cache hits, 0 failed, 0 skipped"
        );
        assert!(first.warnings.is_empty(), "{:?}", first.warnings);

        std::fs::create_dir(root.join("elsewhere")).unwrap();
        std::fs::rename(root.join("lib"), root.join("elsewhere/lib")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere/lib"), root.join("lib")).unwrap();
        let second = run(ws, &graph, &keys, store.clone(), &options(1)).await;
        assert_eq!(
            second.summary(),
            "1 tasks: 1 ran, 0 cache hits, 0 failed, 0 skipped"
        );
        assert_eq!(second.exit_code(), 0);
        let [read, write] = second.warnings.as_slice() else {
            panic!("{:?}", second.warnings);
        };
        assert!(read.starts_with("lib:build: cache read failed, running instead: "));
        assert!(write.starts_with("lib:build: cache write failed: "));
        assert!(read.contains("symlink") && write.contains("symlink"));
        assert!(
            store
                .get(&keys[&TaskId::new("lib", "build")])
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_panicked_job_fails_its_task_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let full = workspace(vec![
            project("lib", &[], &[("build", &[])]),
            project("app", &["lib"], &[("build", &["^build"])]),
        ]);
        let graph = TaskGraph::build(&full, &["build".into()], None).unwrap();
        // The job looks its task up in a workspace without it, and panics.
        let ws = Arc::new(workspace(vec![]));
        let store: Arc<dyn Store> = Arc::new(Local::new(dir.path()));
        let report = run(ws, &graph, &keys(&graph), store, &options(2)).await;
        assert_eq!(
            report.summary(),
            "2 tasks: 0 ran, 0 cache hits, 1 failed, 1 skipped"
        );
        assert_eq!(report.tasks[0].task, TaskId::new("lib", "build"));
        assert_eq!(report.tasks[0].outcome, Outcome::Failed);
        let [warning] = report.warnings.as_slice() else {
            panic!("{:?}", report.warnings);
        };
        assert!(
            warning.starts_with("lib:build: ") && warning.contains("panicked"),
            "{warning}"
        );
    }

    #[cfg(unix)]
    fn is_alive(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 sends nothing; it only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persistent_tasks_take_no_slot_and_die_with_the_run() {
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let server = |name: &str| {
            project(name, &[], &[("dev", &[])]).tap_mut(|p| {
                let dev = p.targets.get_mut("dev").unwrap();
                dev.persistent = true;
                dev.command = format!("sleep 60 & echo $! > ../{name}.pid; wait");
            })
        };
        let mut lib = project("lib", &[], &[("build", &[])]);
        lib.targets.get_mut("build").unwrap().command = "echo built > ../built.txt".into();
        for p in ["a", "b", "lib"] {
            std::fs::create_dir_all(root.join(p)).unwrap();
        }
        let mut ws = workspace(vec![server("a"), server("b"), lib]);
        ws.root = root.clone();
        let roots = vec![
            TaskId::new("a", "dev"),
            TaskId::new("b", "dev"),
            TaskId::new("lib", "build"),
        ];
        let graph = TaskGraph::from_roots(&ws, roots).unwrap();
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        let pid = |name: &str| -> Option<libc::pid_t> {
            std::fs::read_to_string(root.join(format!("{name}.pid")))
                .ok()?
                .trim()
                .parse()
                .ok()
        };
        let started = async {
            loop {
                if let (Some(a), Some(b), true) =
                    (pid("a"), pid("b"), root.join("built.txt").exists())
                {
                    return [a, b];
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let (keys, opts) = (keys(&graph), options(1));
        let sleepers = tokio::select! {
            _ = run(Arc::new(ws), &graph, &keys, store, &opts) => {
                panic!("the persistent tasks finished")
            }
            sleepers = tokio::time::timeout(Duration::from_secs(10), started) => {
                sleepers.expect("every task started with one slot")
            }
        };
        let gone = async {
            while sleepers.iter().any(|&pid| is_alive(pid)) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), gone)
            .await
            .expect("the servers' children died with the run");
    }
}
