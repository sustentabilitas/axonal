mod common;

use common::Repo;
use serde_json::Value;

#[test]
fn committed_change_affects_the_project_and_its_dependents() {
    let repo = Repo::fixture("scripts");
    repo.feature(|r| r.write("libs/a/src/a.txt", "alpha2\n"));
    assert_eq!(repo.stdout(&["affected"]), "apps/b\nlibs/a\n");
    assert_eq!(
        repo.stdout(&["affected", "--target", "build"]),
        "apps/b:build\nlibs/a:build\n"
    );
}

#[test]
fn untracked_worktree_files_count() {
    let repo = Repo::fixture("scripts");
    repo.write("apps/c/src/new.txt", "new\n");
    assert_eq!(repo.stdout(&["affected"]), "apps/c\n");
}

#[test]
fn non_input_changes_affect_nothing() {
    let repo = Repo::fixture("scripts");
    repo.feature(|r| r.write("README.md", "changed\n"));
    assert_eq!(repo.stdout(&["affected"]), "");
}

#[test]
fn config_changes_affect_everything() {
    let repo = Repo::fixture("scripts");
    let config = repo.read("axonal.toml");
    repo.feature(|r| r.write("axonal.toml", &format!("{config}\n# tweak\n")));
    assert_eq!(repo.stdout(&["affected"]), "apps/b\napps/c\nlibs/a\n");
}

#[test]
fn run_affected_runs_only_affected_tasks() {
    let repo = Repo::fixture("scripts");
    repo.feature(|r| r.write("apps/c/src/c.txt", "charlie2\n"));
    let out = repo.stdout(&["run", "build", "--affected"]);
    assert_eq!(
        out.lines().last().unwrap(),
        "1 tasks: 1 ran, 0 cache hits, 0 failed, 0 skipped"
    );
    assert!(out.contains("apps/c:build | built c"), "{out}");
}

#[test]
fn explicit_range_and_json_causes() {
    let repo = Repo::fixture("scripts");
    let base = repo.git(&["rev-parse", "HEAD"]);
    repo.feature(|r| r.write("libs/a/src/a.txt", "alpha2\n"));
    let out = repo.stdout(&["affected", "--base", &base, "--head", "HEAD", "--json"]);
    let report: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["base"], base.as_str());
    assert_eq!(report["projects"], serde_json::json!(["apps/b", "libs/a"]));
    assert_eq!(report["tasks"]["libs/a:build"][0]["kind"], "files");
    assert_eq!(
        report["tasks"]["apps/b:build"][0],
        serde_json::json!({"kind": "dependency", "project": "libs/a"})
    );
}

#[test]
fn a_missing_default_branch_is_an_error() {
    let repo = Repo::fixture("scripts");
    repo.git(&["branch", "-m", "main", "trunk"]);
    let (code, _, stderr) = repo.output(&["affected"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("pass --base"), "{stderr}");
}

#[test]
fn pnpm_lockfile_changes_affect_only_packages_using_the_changed_dependency() {
    let repo = Repo::fixture("mixed");
    let lock = repo
        .read("pnpm-lock.yaml")
        .replace("is-number@6.0.0", "is-number@6.0.1")
        .replace("is-number: 6.0.0", "is-number: 6.0.1");
    repo.feature(|r| r.write("pnpm-lock.yaml", &lock));
    assert_eq!(repo.stdout(&["affected"]), "@mixed/app\n");
}

#[test]
fn cargo_lockfile_changes_affect_crates_using_it_and_their_dependents() {
    let repo = Repo::fixture("mixed");
    let lock = repo.read("Cargo.lock").replace("1.0.15", "1.0.16");
    repo.feature(|r| r.write("Cargo.lock", &lock));
    assert_eq!(repo.stdout(&["affected"]), "engine\nserver\n");
}

#[test]
fn an_unparseable_lockfile_affects_every_js_project() {
    let repo = Repo::fixture("mixed");
    repo.feature(|r| r.write("pnpm-lock.yaml", "lockfileVersion: [\n"));
    assert_eq!(repo.stdout(&["affected"]), "@mixed/app\n@mixed/ui\n");
}

#[test]
fn toolchain_files_affect_their_ecosystem() {
    let repo = Repo::fixture("mixed");
    repo.feature(|r| r.write("rust-toolchain.toml", "[toolchain]\nchannel = \"stable\"\n"));
    assert_eq!(repo.stdout(&["affected"]), "engine\nserver\n");
}

#[test]
fn ts_import_edges_propagate_changes() {
    let repo = Repo::fixture("nx-integrated");
    repo.feature(|r| {
        r.write(
            "libs/money/src/index.ts",
            "export const cents = (a: number) => a * 100;\n",
        )
    });
    assert_eq!(
        repo.stdout(&["affected"]),
        "apps/shop\nlibs/billing\nlibs/feat/checkout\nlibs/money\n"
    );

    let leaf = Repo::fixture("nx-integrated");
    leaf.feature(|r| r.write("apps/shop/src/main.tsx", "export const App = () => null;\n"));
    assert_eq!(leaf.stdout(&["affected"]), "apps/shop\n");
}
