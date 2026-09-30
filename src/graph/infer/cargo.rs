//! Cargo workspaces: members and path dependencies from `cargo metadata --no-deps`.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
};

use serde::Deserialize;

use crate::{
    config::TargetConfig,
    error::{Error, Result},
};

#[derive(Debug, Clone, PartialEq)]
pub struct Crate {
    pub name: String,
    pub root: PathBuf,
    /// Workspace-relative roots of member crates this one depends on by path.
    pub path_deps: BTreeSet<PathBuf>,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    manifest_path: PathBuf,
    #[serde(default)]
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    path: Option<PathBuf>,
}

/// `root` must be canonical: `cargo metadata` reports canonical paths.
pub fn discover(root: &Path) -> Result<Vec<Crate>> {
    if !root.join("Cargo.toml").is_file() {
        return Ok(Vec::new());
    }
    let tool = |message: String| Error::Tool {
        tool: "cargo metadata".into(),
        message,
    };
    let out = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .output()
        .map_err(|e| tool(e.to_string()))?;
    if !out.status.success() {
        return Err(tool(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    let meta: Metadata = serde_json::from_slice(&out.stdout).map_err(|e| tool(e.to_string()))?;
    Ok(from_metadata(root, meta))
}

fn from_metadata(root: &Path, meta: Metadata) -> Vec<Crate> {
    let relative = |p: &Path| p.strip_prefix(root).ok().map(Path::to_path_buf);
    let members: BTreeSet<PathBuf> = meta
        .packages
        .iter()
        .filter_map(|p| p.manifest_path.parent().and_then(relative))
        .collect();
    meta.packages
        .into_iter()
        .filter_map(|p| {
            let crate_root = p.manifest_path.parent().and_then(relative)?;
            let path_deps = p
                .dependencies
                .iter()
                .filter_map(|d| d.path.as_deref().and_then(relative))
                .filter(|d| members.contains(d))
                .collect();
            Some(Crate {
                name: p.name,
                root: crate_root,
                path_deps,
            })
        })
        .collect()
}

/// `build`, `test`, `lint` and `fmt` targets for a member crate.
pub fn targets(name: &str) -> BTreeMap<String, TargetConfig> {
    [
        ("build", format!("cargo build -p {name}")),
        ("test", format!("cargo test -p {name}")),
        (
            "lint",
            format!("cargo clippy -p {name} --all-targets -- -D warnings"),
        ),
        ("fmt", format!("cargo fmt -p {name} -- --check")),
    ]
    .into_iter()
    .map(|(target, command)| {
        let config = TargetConfig {
            command: Some(command),
            ..TargetConfig::default()
        };
        (target.to_string(), config)
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn members_and_path_deps_come_from_metadata() {
        let meta: Metadata = serde_json::from_value(serde_json::json!({"packages": [
            {"name": "core", "manifest_path": "/ws/crates/core/Cargo.toml",
             "dependencies": [{"name": "serde"}]},
            {"name": "cli", "manifest_path": "/ws/crates/cli/Cargo.toml",
             "dependencies": [{"name": "core", "path": "/ws/crates/core"},
                              {"name": "vendored", "path": "/elsewhere/vendored"}]}
        ]}))
        .unwrap();
        let crates = from_metadata(Path::new("/ws"), meta);
        assert!(crates[0].path_deps.is_empty());
        assert_eq!(
            crates[1],
            Crate {
                name: "cli".into(),
                root: "crates/cli".into(),
                path_deps: BTreeSet::from([PathBuf::from("crates/core")]),
            }
        );
    }

    #[test]
    fn no_manifest_means_no_crates() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn discovers_a_real_workspace() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
        );
        write(
            dir.path(),
            "crates/a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        write(dir.path(), "crates/a/src/lib.rs", "");
        write(
            dir.path(),
            "crates/b/Cargo.toml",
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n",
        );
        write(dir.path(), "crates/b/src/lib.rs", "");
        let root = dir.path().canonicalize().unwrap();
        let mut crates = discover(&root).unwrap();
        crates.sort_by(|x, y| x.name.cmp(&y.name));
        assert_eq!(
            crates.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(
            crates[1].path_deps,
            BTreeSet::from([PathBuf::from("crates/a")])
        );
    }

    #[test]
    fn broken_manifest_is_a_tool_error() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.toml", "[package\n");
        let root = dir.path().canonicalize().unwrap();
        assert!(matches!(discover(&root), Err(Error::Tool { .. })));
    }

    #[test]
    fn members_get_cargo_targets() {
        let t = targets("core");
        assert_eq!(
            t.keys().collect::<Vec<_>>(),
            ["build", "fmt", "lint", "test"]
        );
        assert_eq!(t["build"].command.as_deref(), Some("cargo build -p core"));
        assert_eq!(t["test"].command.as_deref(), Some("cargo test -p core"));
        assert_eq!(
            t["lint"].command.as_deref(),
            Some("cargo clippy -p core --all-targets -- -D warnings")
        );
        assert_eq!(
            t["fmt"].command.as_deref(),
            Some("cargo fmt -p core -- --check")
        );
    }
}
