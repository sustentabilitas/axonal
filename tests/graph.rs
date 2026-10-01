mod common;

use common::Repo;
use serde_json::Value;

fn graph(repo: &Repo) -> Value {
    serde_json::from_str(&repo.stdout(&["graph", "--json"])).unwrap()
}

fn names(g: &Value) -> Vec<String> {
    g["projects"].as_object().unwrap().keys().cloned().collect()
}

fn deps(g: &Value, project: &str) -> Vec<String> {
    g["projects"][project]["deps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn pnpm_workspace_packages_scripts_and_links() {
    let g = graph(&Repo::fixture("pnpm"));
    assert_eq!(names(&g), ["@acme/ui", "@acme/web"]);
    assert_eq!(deps(&g, "@acme/web"), ["@acme/ui"]);
    assert_eq!(g["projects"]["@acme/web"]["root"], "apps/web");
    assert_eq!(
        g["projects"]["@acme/web"]["targets"]["test"]["command"],
        "pnpm run test"
    );
}

#[test]
fn cargo_members_get_cargo_targets_and_path_edges() {
    let g = graph(&Repo::fixture("cargo"));
    assert_eq!(names(&g), ["cli-app", "core-lib"]);
    assert_eq!(deps(&g, "cli-app"), ["core-lib"]);
    assert_eq!(
        g["projects"]["core-lib"]["kinds"],
        serde_json::json!(["cargo"])
    );
    assert_eq!(
        g["projects"]["core-lib"]["targets"]["test"]["command"],
        "cargo test -p core-lib"
    );
}

#[test]
fn mixed_workspace_has_both_ecosystems() {
    let g = graph(&Repo::fixture("mixed"));
    assert_eq!(names(&g), ["@mixed/app", "@mixed/ui", "engine", "server"]);
    assert_eq!(deps(&g, "@mixed/app"), ["@mixed/ui"]);
    assert_eq!(deps(&g, "server"), ["engine"]);
}

#[test]
fn integrated_nx_repo_gets_edges_from_ts_imports() {
    let g = graph(&Repo::fixture("nx-integrated"));
    assert!(deps(&g, "libs/money").is_empty());
    assert_eq!(deps(&g, "libs/billing"), ["libs/money"]);
    assert_eq!(deps(&g, "libs/feat/checkout"), ["libs/billing"]);
    assert_eq!(deps(&g, "apps/shop"), ["libs/feat/checkout"]);
}

#[test]
fn dot_and_text_output() {
    let repo = Repo::fixture("cargo");
    assert!(
        repo.stdout(&["graph", "--dot"])
            .contains("  \"cli-app\" -> \"core-lib\";\n")
    );
    let text = repo.stdout(&["graph"]);
    assert!(
        text.contains("core-lib (crates/core)\n  deps: -\n  targets: build, fmt, lint, test"),
        "{text}"
    );
}
