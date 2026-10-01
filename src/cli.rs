//! Command-line interface. Exit codes: 0 success, 1 a task failed, 2 configuration,
//! graph or usage error, 130 interrupted.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::{
    affected::{self, Affected, Range},
    cache::{Local, Store},
    config, git,
    graph::{self, TaskGraph, Workspace},
    hash::{self, FileHashCache, Toolchain},
    init,
    run::{self, RunOptions},
};

#[derive(Parser)]
#[command(
    name = "ax",
    version,
    about = "A fast, simple monorepo task runner for pnpm and Cargo workspaces"
)]
struct Cli {
    /// Run as if ax was started in DIR.
    #[arg(long, global = true, value_name = "DIR")]
    cwd: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write an axonal.toml describing the discovered workspace.
    Init {
        /// Overwrite an existing axonal.toml.
        #[arg(long)]
        force: bool,
    },
    /// Run one or more targets in dependency order.
    Run(RunArgs),
    /// List projects (or tasks, with --target) affected by changes since the base.
    Affected(AffectedArgs),
    /// Print the project graph.
    Graph {
        #[arg(long, conflicts_with = "dot")]
        json: bool,
        #[arg(long)]
        dot: bool,
    },
    /// Inspect or clear the local cache.
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
}

#[derive(Args)]
struct RunArgs {
    /// Targets to run, e.g. `build test`.
    #[arg(required = true)]
    targets: Vec<String>,
    /// Only run targets of these projects (repeatable); their dependencies still run.
    #[arg(short = 'p', long = "project", value_name = "PROJECT")]
    projects: Vec<String>,
    /// Only run tasks affected by changes since the base.
    #[arg(long)]
    affected: bool,
    /// Base revision (default: merge-base with the default branch).
    #[arg(long, requires = "affected")]
    base: Option<String>,
    /// Head revision; changes in the working tree since the base are included as well,
    /// since tasks run against it.
    #[arg(long, requires = "affected")]
    head: Option<String>,
    /// Maximum concurrent tasks (default: CPU cores).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    parallel: Option<u32>,
    /// Keep running independent tasks after a failure.
    #[arg(long = "continue")]
    keep_going: bool,
    /// Ignore the cache: run every task and store nothing.
    #[arg(long)]
    no_cache: bool,
    /// Print a JSON run report on stdout; task output goes to stderr.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct AffectedArgs {
    /// List affected tasks of this target instead of projects.
    #[arg(long)]
    target: Option<String>,
    /// Base revision (default: merge-base with the default branch).
    #[arg(long)]
    base: Option<String>,
    /// Head revision (default: the working tree).
    #[arg(long)]
    head: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum CacheAction {
    /// Show the number and size of local cache entries.
    Stats,
    /// Delete the local cache.
    Clean,
}

pub fn main() -> ExitCode {
    match execute(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn execute(cli: Cli) -> anyhow::Result<u8> {
    let start = std::env::current_dir()?.join(cli.cwd.unwrap_or_default());
    anyhow::ensure!(start.is_dir(), "{} is not a directory", start.display());
    let root = find_root(&start);
    match cli.command {
        Command::Init { force } => init::write(&root, force).map(|()| 0),
        Command::Run(args) => run_command(&root, args),
        Command::Affected(args) => affected_command(&root, args),
        Command::Graph { json, dot } => graph_command(&root, json, dot),
        Command::Cache { action } => cache_command(&root, action),
    }
}

/// The nearest ancestor holding `axonal.toml`, else the git top-level, else `start`.
pub fn find_root(start: &Path) -> PathBuf {
    start
        .ancestors()
        .find(|dir| dir.join(config::FILE).is_file())
        .map(Path::to_path_buf)
        .or_else(|| git::toplevel(start).ok())
        .unwrap_or_else(|| start.to_path_buf())
}

fn run_command(root: &Path, args: RunArgs) -> anyhow::Result<u8> {
    let ws = Workspace::discover(root)?;
    let max_cache = ws.config.cache.local_max_bytes()?;
    let selected: Option<BTreeSet<String>> =
        (!args.projects.is_empty()).then(|| args.projects.iter().cloned().collect());
    let graph = if args.affected {
        let full = TaskGraph::build(&ws, &args.targets, None)?;
        if let Some(names) = &selected {
            ws.check_projects(names)?;
        }
        let range = Range::resolve(&ws, args.base.as_deref(), args.head.as_deref())?;
        // Tasks run against the working tree, so its changes since the base count whatever
        // `--head` names.
        let worktree = Range {
            head: None,
            ..range.clone()
        };
        let mut tasks = affected::from_git(&ws, &full, &worktree)?.tasks;
        if range.head.is_some() {
            tasks.extend(affected::from_git(&ws, &full, &range)?.tasks);
        }
        let roots = tasks
            .keys()
            .filter(|id| args.targets.contains(&id.target))
            .filter(|id| selected.as_ref().is_none_or(|s| s.contains(&id.project)))
            .filter(|id| !ws.target(id).persistent)
            .cloned()
            .collect();
        TaskGraph::from_roots(&ws, roots)?
    } else {
        TaskGraph::build(&ws, &args.targets, selected.as_ref())?
    };

    let mut file_hashes = FileHashCache::load(&ws.root);
    let keys = hash::task_keys(
        &ws,
        &graph,
        &mut file_hashes,
        &Toolchain::detect(&ws),
        &|name| std::env::var_os(name),
    )?;
    file_hashes.save(&ws.root)?;

    let local = Local::new(&ws.root);
    let store: Arc<dyn Store> = Arc::new(local.clone());
    let opts = RunOptions {
        parallel: args
            .parallel
            .map_or_else(run::default_parallelism, |n| n as usize),
        keep_going: args.keep_going,
        use_cache: !args.no_cache,
        json: args.json,
    };
    let report = tokio::runtime::Runtime::new()?.block_on(run::run(
        Arc::new(ws),
        &graph,
        &keys,
        store,
        &opts,
    ));

    let mut warnings = report.warnings.clone();
    if let Err(e) = local.evict(max_cache) {
        warnings.push(format!("cache eviction failed: {e}"));
    }
    warnings.iter().for_each(|w| eprintln!("warning: {w}"));
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", report.summary());
    }
    Ok(if report.interrupted {
        130
    } else {
        report.exit_code()
    })
}

#[derive(Serialize)]
struct AffectedReport<'a> {
    base: &'a str,
    head: Option<&'a str>,
    #[serde(flatten)]
    affected: &'a Affected,
}

fn affected_command(root: &Path, args: AffectedArgs) -> anyhow::Result<u8> {
    let ws = Workspace::discover(root)?;
    let targets = match &args.target {
        Some(target) => vec![target.clone()],
        None => ws.target_names(),
    };
    let graph = TaskGraph::build(&ws, &targets, None)?;
    let range = Range::resolve(&ws, args.base.as_deref(), args.head.as_deref())?;
    let affected = affected::from_git(&ws, &graph, &range)?;
    if args.json {
        let report = AffectedReport {
            base: &range.base,
            head: range.head.as_deref(),
            affected: &affected,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if args.target.is_some() {
        affected.tasks.keys().for_each(|id| println!("{id}"));
    } else {
        affected.projects.iter().for_each(|p| println!("{p}"));
    }
    Ok(0)
}

fn graph_command(root: &Path, json: bool, dot: bool) -> anyhow::Result<u8> {
    let ws = Workspace::discover(root)?;
    if json {
        let value = serde_json::json!({ "projects": &ws.projects });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if dot {
        print!("{}", graph::to_dot(&ws));
    } else {
        let list = |items: Vec<String>| {
            if items.is_empty() {
                "-".to_string()
            } else {
                items.join(", ")
            }
        };
        for p in ws.projects.values() {
            println!(
                "{} ({})\n  deps: {}\n  targets: {}",
                p.name,
                graph::display_root(&p.root),
                list(p.deps.iter().cloned().collect()),
                list(p.targets.keys().cloned().collect()),
            );
        }
    }
    Ok(0)
}

fn cache_command(root: &Path, action: CacheAction) -> anyhow::Result<u8> {
    let local = Local::new(root);
    match action {
        CacheAction::Stats => {
            let stats = local.stats()?;
            println!(
                "{} entries, {:.1} MB in {}",
                stats.entries,
                stats.bytes as f64 / 1_048_576.0,
                local.dir().display()
            );
        }
        CacheAction::Clean => {
            local.clean()?;
            println!("removed {}", local.dir().display());
        }
    }
    Ok(0)
}
