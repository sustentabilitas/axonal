mod common;

use common::Repo;
use serde_json::Value;

fn summary(out: &str) -> &str {
    out.lines().last().unwrap_or_default()
}

#[test]
fn runs_in_dependency_order_and_replays_logs_from_the_cache() {
    let repo = Repo::fixture("scripts");
    let first = repo.stdout(&["run", "build"]);
    assert_eq!(
        summary(&first),
        "3 tasks: 3 ran, 0 cache hits, 0 failed, 0 skipped"
    );
    assert!(first.contains("libs/a:build | built a"), "{first}");
    assert_eq!(repo.read("apps/b/dist/out.txt"), "bravo\nalpha\n");

    let second = repo.stdout(&["run", "build"]);
    assert_eq!(
        summary(&second),
        "3 tasks: 0 ran, 3 cache hits, 0 failed, 0 skipped"
    );
    assert!(second.contains("libs/a:build | built a"), "{second}");
}

#[test]
fn cache_hits_restore_outputs() {
    let repo = Repo::fixture("scripts");
    repo.stdout(&["run", "build"]);
    repo.remove_dir("apps/b/dist");
    let out = repo.stdout(&["run", "build"]);
    assert_eq!(
        summary(&out),
        "3 tasks: 0 ran, 3 cache hits, 0 failed, 0 skipped"
    );
    assert_eq!(repo.read("apps/b/dist/out.txt"), "bravo\nalpha\n");
}

#[test]
fn changing_an_input_reruns_the_task_and_its_dependents() {
    let repo = Repo::fixture("scripts");
    repo.stdout(&["run", "build"]);
    repo.write("libs/a/src/a.txt", "alpha2\n");
    let out = repo.stdout(&["run", "build"]);
    assert_eq!(
        summary(&out),
        "3 tasks: 2 ran, 1 cache hits, 0 failed, 0 skipped"
    );
    assert_eq!(repo.read("apps/b/dist/out.txt"), "bravo\nalpha2\n");
}

#[test]
fn changing_a_non_input_keeps_cache_hits() {
    let repo = Repo::fixture("scripts");
    repo.stdout(&["run", "build"]);
    repo.write("README.md", "changed\n");
    repo.write("libs/a/notes.md", "not an input\n");
    let out = repo.stdout(&["run", "build"]);
    assert_eq!(
        summary(&out),
        "3 tasks: 0 ran, 3 cache hits, 0 failed, 0 skipped"
    );
}

#[test]
fn a_failure_skips_dependents_and_exits_1() {
    let repo = Repo::fixture("scripts");
    let (code, stdout, _) = repo.output(&["run", "deploy"]);
    assert_eq!(code, 1);
    assert!(stdout.contains("apps/b:fail | about to fail"), "{stdout}");
    assert!(!stdout.contains("deploying"), "{stdout}");
    assert_eq!(
        summary(&stdout),
        "2 tasks: 0 ran, 0 cache hits, 1 failed, 1 skipped"
    );
}

#[test]
fn fail_fast_stops_scheduling_new_tasks() {
    let repo = Repo::fixture("scripts");
    let (code, stdout, _) = repo.output(&["run", "fail", "build", "--parallel", "1"]);
    assert_eq!(code, 1);
    assert_eq!(
        summary(&stdout),
        "4 tasks: 0 ran, 0 cache hits, 1 failed, 3 skipped"
    );
}

#[test]
fn continue_runs_independent_tasks() {
    let repo = Repo::fixture("scripts");
    let (code, stdout, _) = repo.output(&["run", "deploy", "build", "--continue"]);
    assert_eq!(code, 1);
    assert_eq!(
        summary(&stdout),
        "5 tasks: 3 ran, 0 cache hits, 1 failed, 1 skipped"
    );
}

#[test]
fn no_cache_runs_everything() {
    let repo = Repo::fixture("scripts");
    repo.stdout(&["run", "build"]);
    let out = repo.stdout(&["run", "build", "--no-cache"]);
    assert_eq!(
        summary(&out),
        "3 tasks: 3 ran, 0 cache hits, 0 failed, 0 skipped"
    );
}

#[test]
fn json_report_on_stdout_and_task_output_on_stderr() {
    let repo = Repo::fixture("scripts");
    let (code, stdout, stderr) = repo.output(&["run", "build", "--json"]);
    assert_eq!(code, 0);
    let report: Value = serde_json::from_str(&stdout).unwrap();
    let mut tasks: Vec<&str> = report["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            assert_eq!(t["outcome"], "ran");
            t["task"].as_str().unwrap()
        })
        .collect();
    tasks.sort();
    assert_eq!(tasks, ["apps/b:build", "apps/c:build", "libs/a:build"]);
    assert!(stderr.contains("libs/a:build | built a"), "{stderr}");
}

#[test]
fn project_filter_runs_dependencies_too() {
    let repo = Repo::fixture("scripts");
    let out = repo.stdout(&["run", "build", "-p", "apps/b"]);
    assert_eq!(
        summary(&out),
        "2 tasks: 2 ran, 0 cache hits, 0 failed, 0 skipped"
    );
}

#[test]
fn unknown_target_exits_2() {
    let (code, _, stderr) = Repo::fixture("scripts").output(&["run", "nope"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("unknown target `nope`"), "{stderr}");
}
