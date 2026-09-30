//! pnpm workspaces: packages from `pnpm-workspace.yaml`, edges from `workspace:` specifiers.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;

use crate::{
    config::TargetConfig,
    error::{Error, Result},
};

pub const WORKSPACE_FILE: &str = "pnpm-workspace.yaml";

#[derive(Debug, Clone, PartialEq)]
pub struct JsPackage {
    pub name: String,
    pub root: PathBuf,
    pub scripts: BTreeMap<String, String>,
    /// Names of packages this one depends on through `workspace:` specifiers.
    pub workspace_deps: BTreeSet<String>,
}

#[derive(Deserialize)]
struct WorkspaceYaml {
    #[serde(default)]
    packages: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PackageJson {
    name: Option<String>,
    #[serde(default)]
    scripts: BTreeMap<String, String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    peer_dependencies: BTreeMap<String, String>,
}

/// `files` is the workspace file list from [`crate::files::list`].
pub fn discover(root: &Path, files: &[PathBuf]) -> Result<Vec<JsPackage>> {
    let path = root.join(WORKSPACE_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let workspace: WorkspaceYaml =
        serde_norway::from_str(&text).map_err(|e| config_error(&path, e))?;
    let clean = |p: &str| p.trim_start_matches("./").trim_end_matches('/').to_string();
    let (excluded, included): (Vec<&String>, Vec<&String>) =
        workspace.packages.iter().partition(|p| p.starts_with('!'));
    let include_root = included
        .iter()
        .any(|p| matches!(clean(p).as_str(), "" | "."));
    let include = glob_set(included.iter().map(|p| clean(p)), &path)?;
    let exclude = glob_set(excluded.iter().map(|p| clean(&p[1..])), &path)?;
    files
        .iter()
        .filter(|f| f.file_name().is_some_and(|n| n == "package.json"))
        .filter_map(|f| f.parent())
        .filter(|dir| {
            if dir.as_os_str().is_empty() {
                include_root
            } else {
                include.is_match(dir) && !exclude.is_match(dir)
            }
        })
        .map(|dir| read_package(root, dir))
        .collect()
}

/// One `pnpm run <script>` target per script.
pub fn targets(scripts: &BTreeMap<String, String>) -> BTreeMap<String, TargetConfig> {
    scripts
        .keys()
        .map(|script| {
            let target = TargetConfig {
                command: Some(format!("pnpm run {}", shell_word(script))),
                ..TargetConfig::default()
            };
            (script.clone(), target)
        })
        .collect()
}

fn shell_word(word: &str) -> String {
    if word
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_:./@".contains(c))
    {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

fn read_package(root: &Path, dir: &Path) -> Result<JsPackage> {
    let path = root.join(dir).join("package.json");
    let pkg: PackageJson = serde_json::from_str(&std::fs::read_to_string(&path)?)
        .map_err(|e| config_error(&path, e))?;
    let workspace_deps = [
        &pkg.dependencies,
        &pkg.dev_dependencies,
        &pkg.optional_dependencies,
        &pkg.peer_dependencies,
    ]
    .into_iter()
    .flatten()
    .filter(|(_, spec)| spec.starts_with("workspace:"))
    .map(|(name, _)| name.clone())
    .collect();
    Ok(JsPackage {
        name: pkg
            .name
            .unwrap_or_else(|| dir.to_string_lossy().into_owned()),
        root: dir.to_path_buf(),
        scripts: pkg.scripts,
        workspace_deps,
    })
}

fn glob_set(globs: impl Iterator<Item = String>, path: &Path) -> Result<GlobSet> {
    globs
        .filter(|g| !matches!(g.as_str(), "" | "."))
        .try_fold(GlobSetBuilder::new(), |mut set, g| {
            set.add(
                GlobBuilder::new(&g)
                    .literal_separator(true)
                    .build()
                    .map_err(|e| config_error(path, e))?,
            );
            Ok::<_, Error>(set)
        })?
        .build()
        .map_err(|e| config_error(path, e))
}

fn config_error(path: &Path, e: impl std::fmt::Display) -> Error {
    Error::Config {
        path: path.to_owned(),
        message: e.to_string(),
    }
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

    fn discover_in(root: &Path) -> Result<Vec<JsPackage>> {
        discover(root, &crate::files::list(root).unwrap())
    }

    #[test]
    fn discovers_included_packages_and_workspace_deps() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            WORKSPACE_FILE,
            "packages:\n  - 'apps/*'\n  - 'libs/**'\n  - '!libs/legacy'\n",
        );
        write(root, "package.json", r#"{"name":"root"}"#);
        write(
            root,
            "apps/web/package.json",
            r#"{"name":"@acme/web","scripts":{"build":"tsc"},
                "dependencies":{"@acme/ui":"workspace:*","react":"^19.0.0"},
                "devDependencies":{"@acme/test-utils":"workspace:^"}}"#,
        );
        write(root, "libs/ui/package.json", r#"{"name":"@acme/ui"}"#);
        write(
            root,
            "libs/nested/deep/package.json",
            r#"{"name":"@acme/deep"}"#,
        );
        write(
            root,
            "libs/legacy/package.json",
            r#"{"name":"@acme/legacy"}"#,
        );
        write(root, "tools/package.json", r#"{"name":"tools"}"#);

        let pkgs = discover_in(root).unwrap();
        let names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["@acme/web", "@acme/deep", "@acme/ui"]);
        let web = &pkgs[0];
        assert_eq!(web.root, PathBuf::from("apps/web"));
        assert_eq!(web.scripts["build"], "tsc");
        assert_eq!(
            web.workspace_deps,
            BTreeSet::from(["@acme/test-utils".to_string(), "@acme/ui".to_string()])
        );
    }

    #[test]
    fn root_package_only_when_listed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), WORKSPACE_FILE, "packages:\n  - '.'\n");
        write(
            dir.path(),
            "package.json",
            r#"{"name":"root","scripts":{"lint":"eslint ."}}"#,
        );
        let pkgs = discover_in(dir.path()).unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].root, PathBuf::new());
    }

    #[test]
    fn packages_without_a_name_are_named_by_path() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), WORKSPACE_FILE, "packages: ['apps/*']\n");
        write(dir.path(), "apps/site/package.json", "{}");
        assert_eq!(discover_in(dir.path()).unwrap()[0].name, "apps/site");
    }

    #[test]
    fn no_workspace_file_means_no_packages() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "package.json", r#"{"name":"solo"}"#);
        assert!(discover_in(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn invalid_yaml_is_a_config_error() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), WORKSPACE_FILE, "packages: [\n");
        assert!(matches!(discover_in(dir.path()), Err(Error::Config { .. })));
    }

    #[test]
    fn scripts_become_pnpm_run_targets() {
        let scripts = BTreeMap::from([
            ("build".to_string(), "tsc".to_string()),
            ("test:unit".to_string(), "vitest".to_string()),
            ("my script".to_string(), "x".to_string()),
        ]);
        let t = targets(&scripts);
        assert_eq!(t["build"].command.as_deref(), Some("pnpm run build"));
        assert_eq!(
            t["test:unit"].command.as_deref(),
            Some("pnpm run test:unit")
        );
        assert_eq!(
            t["my script"].command.as_deref(),
            Some("pnpm run 'my script'")
        );
    }
}
