//! Runs a task graph: dependency order, bounded parallelism, cache restore and save,
//! and output prefixed with `project:target`.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    future::pending,
    io::{self, Write},
    path::Path,
    pin::pin,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use serde::Serialize;
use tap::Tap;
#[cfg(unix)]
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader},
    process::{Child, Command},
    task::{self, JoinError, JoinSet},
};

use crate::{
    cache::{CacheError, LogLine, Meta, Store, archive},
    error::Error,
    files::Patterns,
    graph::{TaskGraph, TaskId, Workspace},
    hash::Key,
};

/// Longest line printed or captured whole; longer ones are split into lines this long.
const MAX_LINE: usize = 64 << 10;
/// Most bytes of output captured for the cache; a task printing more isn't cached.
#[cfg(not(test))]
const MAX_LOGS: usize = 8 << 20;
#[cfg(test)]
const MAX_LOGS: usize = 1 << 20;
/// Bytes counted toward [`MAX_LOGS`] for each line besides its text and newline, roughly
/// what a line costs in memory and in the cached JSON.
const LINE_OVERHEAD: usize = 32;
/// How long stopped tasks get to exit after SIGTERM before they are killed.
const GRACE: Duration = Duration::from_secs(5);
/// How long processes a task left behind may hold its output open after its shell exits
/// before they are killed.
const DRAIN: Duration = Duration::from_millis(200);
/// How often to check whether a shell has exited, should a SIGCHLD go astray.
const EXIT_POLL: Duration = Duration::from_millis(100);

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
    /// Exited non-zero, couldn't start, was stopped by an interrupt, or its runner panicked.
    Failed,
    /// Not run because a dependency failed, or fail-fast or an interrupt stopped scheduling.
    Skipped,
    /// A persistent task axonal stopped, after a failure or on an interrupt.
    Stopped,
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
    /// Whether Ctrl-C or SIGTERM, SIGHUP or SIGQUIT cut the run short.
    pub interrupted: bool,
}

impl RunReport {
    pub fn count(&self, outcome: Outcome) -> usize {
        self.tasks.iter().filter(|t| t.outcome == outcome).count()
    }

    pub fn exit_code(&self) -> u8 {
        u8::from(self.count(Outcome::Failed) + self.count(Outcome::Skipped) > 0)
    }

    /// Stopped tasks are counted only when there are some.
    pub fn summary(&self) -> String {
        let stopped = self.count(Outcome::Stopped);
        format!(
            "{} tasks: {} ran, {} cache hits, {} failed, {} skipped{}",
            self.tasks.len(),
            self.count(Outcome::Ran),
            self.count(Outcome::CacheHit),
            self.count(Outcome::Failed),
            self.count(Outcome::Skipped),
            if stopped > 0 {
                format!(", {stopped} stopped")
            } else {
                String::new()
            },
        )
    }
}

pub fn default_parallelism() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// Runs every task in `graph` once its dependencies have succeeded, at most `opts.parallel`
/// at a time, through `sh -c` (`cmd /C` on Windows) in the project root. Tasks inherit
/// axonal's whole environment, though only their listed `env` variables are hashed, and
/// get a null stdin.
///
/// Persistent tasks are never cached and take no slot, so servers that never exit can't
/// starve the tasks around them; the run ends only once they exit. After a failure, even
/// with `keep_going`, they're stopped as soon as no other task is running or can start.
///
/// On Unix each task runs in its own process group. Stopping tasks sends their groups
/// SIGTERM and gives them [`GRACE`] to exit before killing them; a task whose shell exits
/// while processes it left behind still hold its output has them killed after [`DRAIN`].
/// Ctrl-C (and SIGTERM, SIGHUP and SIGQUIT on Unix) stops every running task: persistent
/// ones are `Stopped`, the rest `Failed`, the unstarted `Skipped`, and a second Ctrl-C
/// kills at once. The signal handlers are installed when `run` is called and stay
/// installed after it returns. Dropping the future kills every running task's group.
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
    let mut interrupts = Interrupts::new();
    let mut s = Scheduler::new(ws, graph, keys, store, opts);
    let mut interrupted = false;
    let mut deadline = None;
    loop {
        s.schedule();
        if deadline.is_none() && s.failed && s.busy == 0 && !s.running.is_empty() {
            deadline = Some(s.stop());
        }
        tokio::select! {
            joined = s.running.join_next_with_id() => match joined {
                Some(joined) => s.finish(joined),
                None => break,
            },
            () = interrupts.recv() => {
                interrupted = true;
                if deadline.is_some() {
                    break;
                }
                s.warnings.push("interrupted: stopping running tasks".into());
                deadline = Some(s.stop());
            }
            () = until(deadline) => break,
        }
    }
    s.kill().await;
    s.report(graph, interrupted)
}

/// Run state: what's ready, running and done.
struct Scheduler<'a> {
    ws: Arc<Workspace>,
    keys: &'a BTreeMap<TaskId, Key>,
    store: Arc<dyn Store>,
    opts: &'a RunOptions,
    dependents: BTreeMap<TaskId, BTreeSet<TaskId>>,
    waiting: BTreeMap<&'a TaskId, usize>,
    ready: VecDeque<TaskId>,
    running: JoinSet<(TaskResult, Vec<String>)>,
    jobs: HashMap<task::Id, Started>,
    /// Running tasks that aren't persistent, which are those taking a slot.
    busy: usize,
    results: BTreeMap<TaskId, TaskResult>,
    warnings: Vec<String>,
    /// No more tasks are started, after a failure without `keep_going` or an interrupt.
    stopped: bool,
    failed: bool,
    groups: Groups,
}

impl<'a> Scheduler<'a> {
    fn new(
        ws: Arc<Workspace>,
        graph: &'a TaskGraph,
        keys: &'a BTreeMap<TaskId, Key>,
        store: Arc<dyn Store>,
        opts: &'a RunOptions,
    ) -> Self {
        Scheduler {
            ws,
            keys,
            store,
            opts,
            dependents: graph.dependents(),
            waiting: graph.deps.iter().map(|(id, d)| (id, d.len())).collect(),
            ready: graph
                .order
                .iter()
                .filter(|id| graph.deps.get(*id).is_none_or(BTreeSet::is_empty))
                .cloned()
                .collect(),
            running: JoinSet::new(),
            jobs: HashMap::new(),
            busy: 0,
            results: BTreeMap::new(),
            warnings: Vec::new(),
            stopped: false,
            failed: false,
            groups: Groups::default(),
        }
    }

    /// Starts every ready persistent task and as many others as there are free slots.
    fn schedule(&mut self) {
        if self.stopped {
            return;
        }
        let ws = &self.ws;
        let (persistent, rest): (VecDeque<_>, VecDeque<_>) = std::mem::take(&mut self.ready)
            .into_iter()
            .partition(|id| is_persistent(ws, id));
        self.ready = rest;
        let free = (self.opts.parallel.max(1))
            .saturating_sub(self.busy)
            .min(self.ready.len());
        let starting: Vec<_> = persistent
            .into_iter()
            .chain(self.ready.drain(..free))
            .collect();
        starting.into_iter().for_each(|id| self.start(id));
    }

    fn start(&mut self, id: TaskId) {
        let persistent = is_persistent(&self.ws, &id);
        self.busy += usize::from(!persistent);
        let job = Job {
            ws: self.ws.clone(),
            key: self.keys[&id].clone(),
            id: id.clone(),
            store: self.store.clone(),
            use_cache: self.opts.use_cache,
            to_stderr: self.opts.json,
            groups: self.groups.clone(),
        };
        let handle = self.running.spawn(job.execute());
        let started = Started {
            task: id,
            persistent,
            at: Instant::now(),
        };
        self.jobs.insert(handle.id(), started);
    }

    /// Records a finished, aborted or panicked job, and readies what its success unblocks.
    fn finish(&mut self, joined: Result<(task::Id, (TaskResult, Vec<String>)), JoinError>) {
        let id = match &joined {
            Ok((id, _)) => *id,
            Err(e) => e.id(),
        };
        let Some(started) = self.jobs.remove(&id) else {
            return;
        };
        self.busy -= usize::from(!started.persistent);
        let (mut result, warnings) = match joined {
            Ok((_, done)) => done,
            Err(e) if e.is_panic() => (
                started.failed(self.keys),
                vec![format!("{}: {e}", started.task)],
            ),
            Err(_) => (started.failed(self.keys), vec![]),
        };
        if started.persistent && self.groups.is_stopping() {
            result.outcome = Outcome::Stopped;
        }
        self.warnings.extend(warnings);
        match result.outcome {
            Outcome::Failed => {
                self.failed = true;
                self.stopped |= !self.opts.keep_going;
            }
            Outcome::Ran | Outcome::CacheHit => {
                for dependent in self.dependents.get(&result.task).into_iter().flatten() {
                    if let Some(n) = self.waiting.get_mut(dependent) {
                        *n -= 1;
                        if *n == 0 {
                            self.ready.push_back(dependent.clone());
                        }
                    }
                }
            }
            Outcome::Skipped | Outcome::Stopped => {}
        }
        self.results.insert(result.task.clone(), result);
    }

    /// Stops scheduling and asks every running task to exit, returning when to kill them.
    fn stop(&mut self) -> tokio::time::Instant {
        self.stopped = true;
        self.groups.stop();
        tokio::time::Instant::now() + GRACE
    }

    /// Kills the tasks still running, keeping the results of any that already finished.
    async fn kill(&mut self) {
        if !self.running.is_empty() {
            self.warnings.push(format!(
                "killed {} tasks that didn't stop",
                self.running.len()
            ));
        }
        self.running.abort_all();
        while let Some(joined) = self.running.join_next_with_id().await {
            self.finish(joined);
        }
    }

    fn report(mut self, graph: &TaskGraph, interrupted: bool) -> RunReport {
        let tasks = graph
            .order
            .iter()
            .map(|id| {
                self.results.remove(id).unwrap_or_else(|| TaskResult {
                    task: id.clone(),
                    outcome: Outcome::Skipped,
                    exit_code: None,
                    duration_ms: 0,
                    key: self.keys[id].clone(),
                })
            })
            .collect();
        RunReport {
            tasks,
            warnings: self.warnings,
            interrupted,
        }
    }
}

fn is_persistent(ws: &Workspace, id: &TaskId) -> bool {
    ws.get_target(id).is_some_and(|t| t.persistent)
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

async fn until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => pending().await,
    }
}

/// Ctrl-C, or SIGINT, SIGTERM, SIGHUP and SIGQUIT on Unix, listened for from creation.
/// Tasks run in their own process groups, so a hangup from a closed terminal reaches only
/// axonal, which must pass it on. Tokio never removes the handlers, so these signals no
/// longer terminate the process afterwards.
struct Interrupts {
    #[cfg(unix)]
    signals: [Option<Signal>; 4],
}

impl Interrupts {
    fn new() -> Interrupts {
        Interrupts {
            #[cfg(unix)]
            signals: [
                SignalKind::interrupt(),
                SignalKind::terminate(),
                SignalKind::hangup(),
                SignalKind::quit(),
            ]
            .map(|k| signal(k).ok()),
        }
    }

    /// The next interrupt; never, if no handler could be installed.
    async fn recv(&mut self) {
        #[cfg(unix)]
        {
            let [int, term, hup, quit] = &mut self.signals;
            tokio::select! {
                () = received(int) => {}
                () = received(term) => {}
                () = received(hup) => {}
                () = received(quit) => {}
            }
        }
        #[cfg(not(unix))]
        if tokio::signal::ctrl_c().await.is_err() {
            pending().await
        }
    }
}

/// The next delivery of `signal`; never, without a handler.
#[cfg(unix)]
async fn received(signal: &mut Option<Signal>) {
    if let Some(signal) = signal
        && signal.recv().await.is_some()
    {
        return;
    }
    pending().await
}

struct Job {
    ws: Arc<Workspace>,
    id: TaskId,
    key: Key,
    store: Arc<dyn Store>,
    use_cache: bool,
    to_stderr: bool,
    groups: Groups,
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
                        .for_each(|line| self.emit(line.text.as_bytes(), line.stderr));
                    return (self.result(Outcome::CacheHit, Some(0), started), warnings);
                }
                Ok(None) => {}
                Err(e) => warnings.push(format!(
                    "{}: cache read failed, running instead: {e:#}",
                    self.id
                )),
            }
        }
        let (outcome, exit_code) = match self.spawn(&target.command, &cwd, cacheable).await {
            Err(e) => {
                let message = format!("failed to start `{}`: {e}", target.command);
                self.emit(message.as_bytes(), true);
                (Outcome::Failed, None)
            }
            Ok((0, logs)) => {
                // A success after SIGTERM may be a task cutting its work short.
                if cacheable && !self.groups.is_stopping() {
                    warnings.extend(self.cache(&target.outputs, logs, started).await);
                }
                (Outcome::Ran, Some(0))
            }
            Ok((code, _)) => (Outcome::Failed, Some(code)),
        };
        (self.result(outcome, exit_code, started), warnings)
    }

    /// Saves a success, or explains why it wasn't. Without `logs`, they overflowed.
    async fn cache(
        &self,
        outputs: &[String],
        logs: Option<Vec<LogLine>>,
        started: Instant,
    ) -> Option<String> {
        let Some(logs) = logs else {
            return Some(format!(
                "{}: outputs not cached: logs exceed {} MiB",
                self.id,
                MAX_LOGS >> 20
            ));
        };
        let saved = self.save(outputs, elapsed_ms(started), logs).await;
        saved.err().map(|e| {
            if is_uncacheable(&e) {
                format!("{}: outputs not cached: {e:#}", self.id)
            } else {
                format!("{}: cache write failed: {e:#}", self.id)
            }
        })
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
    /// Outputs behind a symlink are a silent miss: the save after the run warns.
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
            let stale = match patterns.existing_files(&root, &project_root) {
                Err(Error::SymlinkedOutputPath { .. }) => return Ok(None),
                stale => stale?,
            };
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
        duration_ms: u64,
        logs: Vec<LogLine>,
    ) -> anyhow::Result<()> {
        let patterns = Patterns::new(outputs)?;
        let (store, key, root) = (self.store.clone(), self.key.clone(), self.ws.root.clone());
        let project_root = self.ws.projects[&self.id.project].root.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let files = patterns.existing_files(&root, &project_root)?;
            let packed = archive::pack(&root, &files, 0)?;
            let meta = Meta::new(key.clone(), 0, duration_ms, logs, &packed);
            Ok(store.put(&key, &meta, packed.path())?)
        })
        .await?
    }

    /// Runs `command`, printing its output and, when `capture`, keeping it for the cache:
    /// the logs are `None` if not captured or past [`MAX_LOGS`].
    async fn spawn(
        &self,
        command: &str,
        cwd: &Path,
        capture: bool,
    ) -> io::Result<(i32, Option<Vec<LogLine>>)> {
        let mut child = shell(command)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let group = self.groups.track(&child);
        let logs = capture.then(|| Mutex::new(Capture::new()));
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let pumped = {
            let mut pumps = pin!(async {
                let (out, err) = tokio::join!(
                    self.pump(stdout, logs.as_ref(), false),
                    self.pump(stderr, logs.as_ref(), true)
                );
                out.and(err)
            });
            let drained = tokio::select! {
                pumped = &mut pumps => Some(pumped),
                exited = exited(&mut child) => {
                    exited?;
                    tokio::time::timeout(DRAIN, &mut pumps).await.ok()
                }
            };
            match drained {
                Some(pumped) => pumped,
                // Processes the shell left behind hold the output open.
                None => {
                    group.kill();
                    tokio::time::timeout(DRAIN, &mut pumps)
                        .await
                        .unwrap_or(Ok(()))
                }
            }
        };
        exited(&mut child).await?;
        let status = group.reap(&mut child)?;
        pumped?;
        let logs = logs.and_then(|logs| {
            logs.into_inner()
                .unwrap_or_else(PoisonError::into_inner)
                .lines
        });
        Ok((exit_code(status), logs))
    }

    /// Prints and captures `stream` line by line, splitting lines at [`MAX_LINE`] bytes,
    /// between UTF-8 characters where there are any.
    async fn pump(
        &self,
        stream: Option<impl AsyncRead + Unpin>,
        logs: Option<&Mutex<Capture>>,
        stderr: bool,
    ) -> io::Result<()> {
        let Some(stream) = stream else {
            return Ok(());
        };
        let mut reader = BufReader::new(stream);
        // Starts with the bytes of a character the last chunk would have split.
        let mut buf = Vec::new();
        // The newline ending a split line may come alone, ending nothing more.
        let mut split = false;
        loop {
            let room = (MAX_LINE - buf.len()) as u64;
            let read = (&mut reader).take(room).read_until(b'\n', &mut buf).await?;
            if read == 0 && buf.is_empty() {
                return Ok(());
            }
            let ended = buf.ends_with(b"\n");
            let end = if buf.len() == MAX_LINE && !ended {
                char_boundary(&buf)
            } else {
                buf.len()
            };
            let line = buf[..end].strip_suffix(b"\n").unwrap_or(&buf[..end]);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if !(split && ended && line.is_empty()) {
                self.emit(line, stderr);
                if let Some(logs) = logs {
                    logs.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(stderr, line);
                }
            }
            split = !ended;
            buf.drain(..end);
        }
    }

    /// Writes `line` as is, prefixed with the task.
    fn emit(&self, line: &[u8], stderr: bool) {
        let text = [format!("{} | ", self.id).as_bytes(), line, b"\n"].concat();
        let _ = if stderr || self.to_stderr {
            io::stderr().lock().write_all(&text)
        } else {
            io::stdout().lock().write_all(&text)
        };
    }
}

/// The length of `chunk` without the UTF-8 character it cuts short at its end, if any.
fn char_boundary(chunk: &[u8]) -> usize {
    (chunk.len().saturating_sub(3)..chunk.len())
        .rev()
        .find(|&i| chunk[i] & 0xC0 != 0x80)
        .filter(|&i| std::str::from_utf8(&chunk[i..]).is_err_and(|e| e.error_len().is_none()))
        .unwrap_or(chunk.len())
}

/// Output lines captured for the cache, abandoned past [`MAX_LOGS`] bytes, counting each
/// line's newline and [`LINE_OVERHEAD`], so a replay is never truncated.
struct Capture {
    bytes: usize,
    lines: Option<Vec<LogLine>>,
}

impl Capture {
    fn new() -> Capture {
        Capture {
            bytes: 0,
            lines: Some(Vec::new()),
        }
    }

    fn push(&mut self, stderr: bool, line: &[u8]) {
        self.bytes = self.bytes.saturating_add(line.len() + 1 + LINE_OVERHEAD);
        if self.bytes > MAX_LOGS {
            self.lines = None;
        }
        if let Some(lines) = &mut self.lines {
            lines.push(LogLine {
                stderr,
                text: String::from_utf8_lossy(line).into_owned(),
            });
        }
    }
}

/// Outputs that can't be cached, behind or being symlinks or special files: the save is
/// skipped, not failed.
fn is_uncacheable(e: &anyhow::Error) -> bool {
    e.downcast_ref::<CacheError>()
        .is_some_and(|e| matches!(e, CacheError::Symlink(_) | CacheError::NotAFile(_)))
        || e.downcast_ref::<Error>()
            .is_some_and(|e| matches!(e, Error::SymlinkedOutputPath { .. }))
}

/// The exit code, or 128 plus the signal that killed the process, as shells report it.
fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal = None;
    status
        .code()
        .or_else(|| signal.map(|s| 128 + s))
        .unwrap_or(-1)
}

/// The process groups of running tasks, by id: the pid of the task's shell, its leader.
/// A group is listed only until its shell is reaped, which happens under the lock, so a
/// listed id can't have been reused.
#[derive(Clone, Default)]
struct Groups(Arc<Mutex<Live>>);

#[derive(Default)]
struct Live {
    pgids: BTreeSet<u32>,
    /// Groups get SIGTERM, those listed now and any listed later.
    stopping: bool,
}

impl Groups {
    fn lock(&self) -> MutexGuard<'_, Live> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn track(&self, child: &Child) -> Group {
        let pgid = child.id();
        let mut live = self.lock();
        if let Some(pgid) = pgid {
            live.pgids.insert(pgid);
            if live.stopping {
                killpg(pgid, GroupSignal::Term);
            }
        }
        Group {
            groups: self.clone(),
            pgid,
        }
    }

    /// Sends every group SIGTERM, from now on.
    fn stop(&self) {
        let mut live = self.lock();
        live.stopping = true;
        live.pgids
            .iter()
            .for_each(|&pgid| killpg(pgid, GroupSignal::Term));
    }

    fn is_stopping(&self) -> bool {
        self.lock().stopping
    }
}

/// A task's tracked process group, killed with everything in it if dropped before its
/// shell is reaped, e.g. when the run is dropped or kills tasks that didn't stop.
struct Group {
    groups: Groups,
    pgid: Option<u32>,
}

impl Group {
    /// Kills what's left in the group; the shell must not have been reaped.
    fn kill(&self) {
        if let Some(pgid) = self.pgid {
            killpg(pgid, GroupSignal::Kill);
        }
    }

    /// Reaps the shell, which must have exited, untracking its group.
    fn reap(mut self, child: &mut Child) -> io::Result<ExitStatus> {
        let mut live = self.groups.lock();
        if let Some(pgid) = self.pgid.take() {
            live.pgids.remove(&pgid);
        }
        child
            .try_wait()?
            .ok_or_else(|| io::Error::other("the task's shell exited but wasn't reaped"))
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            let mut live = self.groups.lock();
            live.pgids.remove(&pgid);
            killpg(pgid, GroupSignal::Kill);
        }
    }
}

#[derive(Clone, Copy)]
enum GroupSignal {
    Term,
    Kill,
}

/// Signals a tracked process group (a no-op off Unix). ESRCH, when no member is left, is
/// harmless.
fn killpg(pgid: u32, signal: GroupSignal) {
    #[cfg(unix)]
    if let Ok(pgid) = libc::pid_t::try_from(pgid) {
        let signal = match signal {
            GroupSignal::Term => libc::SIGTERM,
            GroupSignal::Kill => libc::SIGKILL,
        };
        // SAFETY: killpg only sends a signal, to a group whose leader is unreaped.
        unsafe { libc::killpg(pgid, signal) };
    }
    #[cfg(not(unix))]
    let _ = (pgid, signal);
}

/// Waits for `child` to exit without reaping it, so its pid, the group id, stays reserved
/// until [`Group::reap`].
#[cfg(unix)]
async fn exited(child: &mut Child) -> io::Result<()> {
    let Some(pid) = child.id() else {
        return Ok(());
    };
    let mut sigchld = signal(SignalKind::child()).ok();
    while !has_exited(pid)? {
        tokio::select! {
            () = received(&mut sigchld) => {}
            () = tokio::time::sleep(EXIT_POLL) => {}
        }
    }
    Ok(())
}

#[cfg(not(unix))]
async fn exited(child: &mut Child) -> io::Result<()> {
    child.wait().await.map(drop)
}

#[cfg(unix)]
fn has_exited(pid: u32) -> io::Result<bool> {
    // SAFETY: `siginfo_t` is plain data, for which all zeroes is a valid value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let options = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    // SAFETY: `info` is a valid, writable `siginfo_t`; WNOWAIT leaves the child waitable.
    match unsafe { libc::waitid(libc::P_PID, pid, &mut info, options) } {
        // SAFETY: waitid succeeded, so `info` is initialised; with WNOHANG `si_pid` stays
        // 0 while the child runs.
        0 => Ok(unsafe { info.si_pid() } != 0),
        _ => match io::Error::last_os_error() {
            e if e.kind() == io::ErrorKind::Interrupted => Ok(false),
            e => Err(e),
        },
    }
}

/// `cmd /C`, passed `command` verbatim since `cmd` doesn't parse arguments the usual way.
#[cfg(windows)]
fn shell(command: &str) -> Command {
    Command::new("cmd").tap_mut(|cmd| {
        cmd.arg("/C").raw_arg(command);
    })
}

/// `sh -c`, in a new process group on Unix.
#[cfg(not(windows))]
fn shell(command: &str) -> Command {
    Command::new("sh").tap_mut(|cmd| {
        cmd.args(["-c", command]);
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
            interrupted: false,
        };
        assert_eq!(
            report.summary(),
            "4 tasks: 1 ran, 1 cache hits, 1 failed, 1 skipped"
        );
        assert_eq!(report.exit_code(), 1);
        assert_eq!(RunReport::default().exit_code(), 0);
    }

    #[test]
    fn summary_appends_stopped_tasks_which_dont_fail_the_run() {
        let report = RunReport {
            tasks: vec![result("a", Outcome::Ran), result("b", Outcome::Stopped)],
            ..RunReport::default()
        };
        assert_eq!(
            report.summary(),
            "2 tasks: 1 ran, 0 cache hits, 0 failed, 0 skipped, 1 stopped"
        );
        assert_eq!(report.exit_code(), 0);
    }

    /// A project whose persistent `dev` target runs `command`.
    fn server(name: &str, command: &str) -> crate::graph::Project {
        project(name, &[], &[("dev", &[])]).tap_mut(|p| {
            let dev = p.targets.get_mut("dev").unwrap();
            dev.persistent = true;
            dev.command = command.into();
        })
    }

    fn build(name: &str, command: &str) -> crate::graph::Project {
        project(name, &[], &[("build", &[])])
            .tap_mut(|p| p.targets.get_mut("build").unwrap().command = command.into())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fail_fast_stops_persistent_tasks() {
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ws = ws_at(
            &root,
            vec![server("srv", "sleep 30"), build("bad", "exit 3")],
        );
        let roots = vec![TaskId::new("srv", "dev"), TaskId::new("bad", "build")];
        let graph = TaskGraph::from_roots(&ws, roots).unwrap();
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        let report = tokio::time::timeout(
            Duration::from_secs(3),
            run(Arc::new(ws), &graph, &keys(&graph), store, &options(1)),
        )
        .await
        .expect("fail-fast run hung behind a persistent task");
        assert_eq!(
            report.summary(),
            "2 tasks: 0 ran, 0 cache hits, 1 failed, 0 skipped, 1 stopped"
        );
        assert_eq!(report.exit_code(), 1);
        assert!(!report.interrupted);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn keep_going_stops_persistent_tasks_once_the_rest_are_done_after_a_failure() {
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ws = ws_at(
            &root,
            vec![
                server("srv", "sleep 30"),
                build("bad", "exit 3"),
                build("good", "true"),
            ],
        );
        let roots = vec![
            TaskId::new("srv", "dev"),
            TaskId::new("bad", "build"),
            TaskId::new("good", "build"),
        ];
        let graph = TaskGraph::from_roots(&ws, roots).unwrap();
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        let opts = RunOptions {
            keep_going: true,
            ..options(1)
        };
        let report = tokio::time::timeout(
            Duration::from_secs(3),
            run(Arc::new(ws), &graph, &keys(&graph), store, &opts),
        )
        .await
        .expect("the run hung behind a persistent task");
        assert_eq!(
            report.summary(),
            "3 tasks: 1 ran, 0 cache hits, 1 failed, 0 skipped, 1 stopped"
        );
        assert!(!report.interrupted);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_lets_persistent_tasks_shut_down_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let srv = server(
            "srv",
            "trap 'echo bye > ../t; exit 0' TERM; touch ../ready; sleep 60 & wait",
        );
        let bad = build(
            "bad",
            "while [ ! -f ../ready ]; do sleep 0.01; done; exit 3",
        );
        let ws = ws_at(&root, vec![srv, bad]);
        let roots = vec![TaskId::new("srv", "dev"), TaskId::new("bad", "build")];
        let graph = TaskGraph::from_roots(&ws, roots).unwrap();
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        let started = Instant::now();
        let report = run(Arc::new(ws), &graph, &keys(&graph), store, &options(1)).await;
        assert!(started.elapsed() < GRACE, "{:?}", started.elapsed());
        assert_eq!(std::fs::read_to_string(root.join("t")).unwrap(), "bye\n");
        let srv = report
            .tasks
            .iter()
            .find(|t| t.task.project == "srv")
            .unwrap();
        assert_eq!((srv.outcome, srv.exit_code), (Outcome::Stopped, Some(0)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_children_holding_the_output_dont_hold_up_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (ws, graph) = single(&root, "sleep 5 & echo $! > ../bg.pid; echo done");
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        let started = Instant::now();
        let report = run(ws, &graph, &keys(&graph), store.clone(), &options(1)).await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(report.tasks[0].outcome, Outcome::Ran);
        assert_eq!(stored(&store, &graph).unwrap().logs, [line(false, "done")]);
        let bg = std::fs::read_to_string(root.join("bg.pid")).unwrap();
        let bg = bg.trim().parse().unwrap();
        let gone = async {
            while is_alive(bg) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), gone)
            .await
            .expect("the background child was killed");
    }

    /// Where [`signalled_run`] runs, set only in the test binary it re-executes.
    #[cfg(unix)]
    const SIGNALLED_ROOT: &str = "AXONAL_TEST_SIGNALLED_ROOT";

    /// A server and a slow build run until a signal stops them, written up to `report`.
    /// Does nothing unless re-executed by [`signal_stops_every_task`], so that the signal
    /// reaches no other test.
    #[cfg(unix)]
    #[tokio::test]
    async fn signalled_run() {
        let Some(root) = std::env::var_os(SIGNALLED_ROOT) else {
            return;
        };
        let root = Path::new(&root);
        let srv = server(
            "srv",
            "trap 'echo bye > ../t; exit 0' TERM; echo $$ > ../srv.pid; sleep 60 & wait",
        );
        let slow = build("slow", "echo $$ > ../slow.pid; sleep 60");
        let ws = ws_at(root, vec![srv, slow]);
        let roots = vec![TaskId::new("srv", "dev"), TaskId::new("slow", "build")];
        let graph = TaskGraph::from_roots(&ws, roots).unwrap();
        let store: Arc<dyn Store> = Arc::new(Local::new(root));
        let opts = options(2);
        let report = run(Arc::new(ws), &graph, &keys(&graph), store, &opts).await;
        let interrupted = if report.interrupted {
            "interrupted"
        } else {
            ""
        };
        std::fs::write(
            root.join("report"),
            format!("{}; {interrupted}", report.summary()),
        )
        .unwrap();
    }

    /// Runs [`signalled_run`] in a subprocess, sends it `signal` once its tasks have
    /// started, and checks that it stopped them all, their process groups included.
    #[cfg(unix)]
    fn signal_stops_every_task(signal: libc::c_int) {
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "run::tests::signalled_run", "--nocapture"])
            .env(SIGNALLED_ROOT, &root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = |name: &str| -> Option<libc::pid_t> {
            std::fs::read_to_string(root.join(format!("{name}.pid")))
                .ok()?
                .trim()
                .parse()
                .ok()
        };
        let mut groups = None;
        let started = within(Duration::from_secs(10), || {
            groups = pid("srv").zip(pid("slow")).map(|(a, b)| [a, b]);
            groups.is_some()
        });
        let child_pid = libc::pid_t::try_from(child.id()).unwrap();
        // SAFETY: only signals the child, which handles it once its tasks have started.
        unsafe { libc::kill(child_pid, signal) };
        let exited = within(Duration::from_secs(10), || {
            child.try_wait().unwrap().is_some()
        });
        let groups = groups.unwrap_or_default();
        let gone = within(Duration::from_secs(5), || {
            !groups.iter().any(|&g| group_exists(g))
        });
        groups.iter().for_each(|&g| {
            // SAFETY: only signals the groups of the tasks this test started.
            unsafe { libc::killpg(g, libc::SIGKILL) };
        });
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(started, "the tasks didn't start: {log}");
        assert!(
            exited && output.status.success(),
            "{:?}: {log}",
            output.status
        );
        assert!(gone, "task process groups outlived the run: {log}");
        assert_eq!(
            std::fs::read_to_string(root.join("report")).unwrap(),
            "2 tasks: 0 ran, 0 cache hits, 1 failed, 0 skipped, 1 stopped; interrupted"
        );
        assert_eq!(std::fs::read_to_string(root.join("t")).unwrap(), "bye\n");
    }

    /// Whether `done` holds within `limit`, checking it every 10 ms.
    #[cfg(unix)]
    fn within(limit: std::time::Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while !done() {
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        true
    }

    #[cfg(unix)]
    fn group_exists(pgid: libc::pid_t) -> bool {
        // SAFETY: signal 0 sends nothing; it only checks that a member exists.
        unsafe { libc::killpg(pgid, 0) == 0 }
    }

    #[cfg(unix)]
    #[test]
    fn ctrl_c_stops_persistent_tasks_and_fails_the_rest() {
        signal_stops_every_task(libc::SIGINT);
    }

    #[cfg(unix)]
    #[test]
    fn sigterm_stops_every_task() {
        signal_stops_every_task(libc::SIGTERM);
    }

    /// Closing the terminal signals axonal's process group, not its tasks' own groups.
    #[cfg(unix)]
    #[test]
    fn sighup_stops_every_task() {
        signal_stops_every_task(libc::SIGHUP);
    }

    #[cfg(unix)]
    #[test]
    fn sigquit_stops_every_task() {
        signal_stops_every_task(libc::SIGQUIT);
    }

    #[cfg(unix)]
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
    async fn logs_over_the_cap_are_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let lines = MAX_LOGS / 99 + 1;
        let (ws, graph) = single(&root, &format!("yes {} | head -n {lines}", "x".repeat(99)));
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        let report = run(ws, &graph, &keys(&graph), store.clone(), &options(1)).await;
        assert_eq!(report.tasks[0].outcome, Outcome::Ran);
        assert_eq!(
            report.warnings,
            [format!(
                "p:build: outputs not cached: logs exceed {} MiB",
                MAX_LOGS >> 20
            )]
        );
        assert!(stored(&store, &graph).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn blank_lines_count_toward_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let lines = MAX_LOGS / (1 + LINE_OVERHEAD) + 1;
        let (ws, graph) = single(&root, &format!("yes '' | head -n {lines}"));
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        let report = run(ws, &graph, &keys(&graph), store.clone(), &options(1)).await;
        assert_eq!(report.tasks[0].outcome, Outcome::Ran);
        assert_eq!(
            report.warnings,
            [format!(
                "p:build: outputs not cached: logs exceed {} MiB",
                MAX_LOGS >> 20
            )]
        );
        assert!(stored(&store, &graph).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn long_lines_are_split_into_bounded_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (ws, graph) = single(&root, "head -c 1000000 /dev/zero | tr '\\0' a");
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        let report = run(ws, &graph, &keys(&graph), store.clone(), &options(1)).await;
        assert_eq!(report.tasks[0].outcome, Outcome::Ran);
        let logs = stored(&store, &graph).unwrap().logs;
        assert_eq!(logs.len(), 1_000_000_usize.div_ceil(MAX_LINE));
        assert!(logs.iter().all(|l| !l.stderr && l.text.len() <= MAX_LINE));
        assert_eq!(logs.iter().map(|l| l.text.len()).sum::<usize>(), 1_000_000);
    }

    #[test]
    fn char_boundary_drops_only_a_character_cut_short() {
        let (euro, smile) = ("€".as_bytes(), "😀".as_bytes());
        assert_eq!(char_boundary(b"ab"), 2);
        assert_eq!(char_boundary(&[b"a", &euro[..2]].concat()), 1);
        assert_eq!(char_boundary(&[b"a", euro].concat()), 4);
        assert_eq!(char_boundary(&[b"a", &smile[..3]].concat()), 1);
        assert_eq!(char_boundary(b"a\x80\x80\x80"), 4);
        assert_eq!(char_boundary(b"a\xff"), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_line_of_exactly_the_limit_stays_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let command = format!("head -c {MAX_LINE} /dev/zero | tr '\\0' a; echo; echo next");
        let (ws, graph) = single(&root, &command);
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        run(ws, &graph, &keys(&graph), store.clone(), &options(1)).await;
        assert_eq!(
            stored(&store, &graph).unwrap().logs,
            [line(false, &"a".repeat(MAX_LINE)), line(false, "next")]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn long_lines_are_split_between_utf8_characters() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let command = format!(
            "head -c {} /dev/zero | tr '\\0' a; printf '\\303\\251\\n'",
            MAX_LINE - 1
        );
        let (ws, graph) = single(&root, &command);
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        run(ws, &graph, &keys(&graph), store.clone(), &options(1)).await;
        assert_eq!(
            stored(&store, &graph).unwrap().logs,
            [line(false, &"a".repeat(MAX_LINE - 1)), line(false, "é")]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failures_are_never_cached_and_output_needs_no_newline_or_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (ws, graph) = single(
            &root,
            r"printf 'a\377b\nno-newline'; printf 'err\n' >&2; exit 2",
        );
        let keys = keys(&graph);
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        for _ in 0..2 {
            let r = run(ws.clone(), &graph, &keys, store.clone(), &options(1)).await;
            assert_eq!(r.tasks[0].outcome, Outcome::Failed);
            assert_eq!(r.tasks[0].exit_code, Some(2));
        }
        assert!(stored(&store, &graph).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_signal_death_exits_128_plus_the_signal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (ws, graph) = single(&root, "kill -TERM $$");
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        let report = run(ws, &graph, &keys(&graph), store, &options(1)).await;
        assert_eq!(report.tasks[0].outcome, Outcome::Failed);
        assert_eq!(report.tasks[0].exit_code, Some(128 + 15));
    }

    #[tokio::test]
    async fn a_task_that_cannot_start_has_no_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (ws, graph) = single(&root, "true");
        std::fs::remove_dir(root.join("p")).unwrap();
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        let report = run(ws, &graph, &keys(&graph), store, &options(1)).await;
        assert_eq!(report.tasks[0].outcome, Outcome::Failed);
        assert_eq!(report.tasks[0].exit_code, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn keep_going_skips_dependents_of_failures_transitively_even_at_parallel_0() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let build = |name: &str, deps: &[&str], command: &str| {
            let depends_on: &[&str] = if deps.is_empty() { &[] } else { &["^build"] };
            project(name, deps, &[("build", depends_on)])
                .tap_mut(|p| p.targets.get_mut("build").unwrap().command = command.into())
        };
        let ws = ws_at(
            &root,
            vec![
                build("util", &[], "exit 1"),
                build("lib", &["util"], "true"),
                build("app", &["lib"], "true"),
                build("tool", &[], "true"),
            ],
        );
        let graph = TaskGraph::build(&ws, &["build".into()], None).unwrap();
        let store: Arc<dyn Store> = Arc::new(Local::new(&root));
        let opts = RunOptions {
            parallel: 0,
            keep_going: true,
            use_cache: false,
            json: true,
        };
        let report = run(Arc::new(ws), &graph, &keys(&graph), store, &opts).await;
        assert_eq!(
            report.summary(),
            "4 tasks: 1 ran, 0 cache hits, 1 failed, 2 skipped"
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
        let [warning] = second.warnings.as_slice() else {
            panic!("{:?}", second.warnings);
        };
        assert!(
            warning.starts_with("lib:build: outputs not cached: output path lib is a symlink"),
            "{warning}"
        );
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
