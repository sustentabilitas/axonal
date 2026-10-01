mod common;

use common::Repo;

#[test]
fn init_writes_a_config_and_refuses_to_overwrite() {
    let repo = Repo::fixture("pnpm");
    let out = repo.stdout(&["init"]);
    assert!(out.starts_with("wrote "), "{out}");
    let config = repo.read("axonal.toml");
    assert!(config.contains("#   @acme/ui (libs/ui, js)\n"), "{config}");
    assert!(config.contains("default_branch = \"main\""), "{config}");
    repo.stdout(&["graph"]);

    let (code, _, stderr) = repo.output(&["init"]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("already exists (pass --force to overwrite)"),
        "{stderr}"
    );
    repo.stdout(&["init", "--force"]);
}

#[test]
fn cache_stats_and_clean() {
    let repo = Repo::fixture("scripts");
    repo.stdout(&["run", "build"]);
    assert!(repo.stdout(&["cache", "stats"]).starts_with("3 entries, "));
    assert!(repo.stdout(&["cache", "clean"]).starts_with("removed "));
    assert!(repo.stdout(&["cache", "stats"]).starts_with("0 entries, "));
}

#[test]
fn the_root_is_found_from_a_subdirectory_and_with_cwd() {
    let repo = Repo::fixture("scripts");
    let expected = repo.stdout(&["graph", "--dot"]);
    let out = repo
        .ax()
        .current_dir(repo.path().join("apps/b"))
        .args(["graph", "--dot"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), expected);
    assert_eq!(
        repo.stdout(&["--cwd", "apps/b", "graph", "--dot"]),
        expected
    );
}

#[test]
fn base_without_affected_is_a_usage_error() {
    let (code, _, stderr) = Repo::fixture("scripts").output(&["run", "build", "--base", "main"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("--affected"), "{stderr}");
}

#[test]
fn config_errors_exit_2_with_the_file_name() {
    let repo = Repo::fixture("scripts");
    repo.write("axonal.toml", "[targets.build]\nimputs = [\"src/**\"]\n");
    let (code, _, stderr) = repo.output(&["graph"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("axonal.toml"), "{stderr}");
    assert!(stderr.contains("imputs"), "{stderr}");
}

#[test]
fn dependency_cycles_exit_2_and_name_the_cycle() {
    let repo = Repo::fixture("scripts");
    let config = repo.read("axonal.toml").replace(
        "[projects.\"libs/a\".targets.build]",
        "[projects.\"libs/a\"]\ndeps = [\"apps/b\"]\n\n[projects.\"libs/a\".targets.build]",
    );
    repo.write("axonal.toml", &config);
    let (code, _, stderr) = repo.output(&["run", "build"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("dependency cycle: "), "{stderr}");
    assert!(
        stderr.contains("apps/b:build") && stderr.contains("libs/a:build"),
        "{stderr}"
    );
}

#[test]
fn unknown_project_filters_exit_2() {
    let (code, _, stderr) = Repo::fixture("scripts").output(&["run", "build", "-p", "apps/nope"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("unknown project `apps/nope`"), "{stderr}");
}
