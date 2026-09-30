//! Projects, dependencies and targets: inferred from pnpm, Cargo and TS imports, then
//! overridden and extended by `axonal.toml`.

pub mod infer;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use serde::Serialize;

pub use crate::files::display_root;
use crate::{
    config::{self, Config, DepsUsage, ProjectConfig, TargetConfig},
    error::{Error, Result},
    files::{self, Owners},
};
use infer::{cargo, pnpm, ts};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Js,
    Cargo,
    Explicit,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Js => "js",
            Kind::Cargo => "cargo",
            Kind::Explicit => "explicit",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Target {
    pub command: String,
    pub depends_on: Vec<String>,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub env: Vec<String>,
    pub deps_usage: DepsUsage,
    pub persistent: bool,
}

impl Target {
    fn finalize(project: &str, name: &str, config: TargetConfig) -> Result<Target> {
        Ok(Target {
            command: config.command.ok_or_else(|| Error::MissingCommand {
                project: project.into(),
                target: name.into(),
            })?,
            depends_on: config.depends_on.unwrap_or_default(),
            inputs: config.inputs.unwrap_or_else(|| vec!["**/*".into()]),
            outputs: config.outputs.unwrap_or_default(),
            env: config.env.unwrap_or_default(),
            deps_usage: config
                .deps_usage
                .unwrap_or_else(|| DepsUsage::default_for(name)),
            persistent: config.persistent.unwrap_or(false),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub name: String,
    pub root: PathBuf,
    pub kinds: BTreeSet<Kind>,
    pub deps: BTreeSet<String>,
    /// Dev-only edges (Cargo dev-dependencies): they feed cache keys and affected detection
    /// but never order tasks, so cargo-legal dev cycles can't become task cycles.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub dev_deps: BTreeSet<String>,
    pub targets: BTreeMap<String, Target>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crate_name: Option<String>,
    /// Files this project owns (deepest root wins), workspace-relative and sorted.
    #[serde(skip)]
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    /// Canonical absolute path.
    pub root: PathBuf,
    pub config: Config,
    pub projects: BTreeMap<String, Project>,
    /// Every non-ignored file, workspace-relative and sorted.
    pub files: Vec<PathBuf>,
}

#[derive(Default)]
struct Draft {
    js_name: Option<String>,
    crate_name: Option<String>,
    explicit: bool,
    inferred: BTreeMap<String, TargetConfig>,
    deps: BTreeSet<PathBuf>,
    dev_deps: BTreeSet<PathBuf>,
}

impl Draft {
    fn kinds(&self) -> BTreeSet<Kind> {
        [
            (self.js_name.is_some(), Kind::Js),
            (self.crate_name.is_some(), Kind::Cargo),
            (self.explicit, Kind::Explicit),
        ]
        .into_iter()
        .filter_map(|(present, kind)| present.then_some(kind))
        .collect()
    }
}

impl Workspace {
    pub fn discover(root: &Path) -> Result<Workspace> {
        let root = root.canonicalize()?;
        let config = Config::load(&root)?;
        let files = files::list(&root)?;
        let declared = declared_projects(&config)?;

        let mut drafts: BTreeMap<PathBuf, Draft> = BTreeMap::new();
        for c in cargo::discover(&root)? {
            let draft = drafts.entry(c.root).or_default();
            draft.inferred.extend(cargo::targets(&c.name));
            draft.deps.extend(c.path_deps);
            draft.dev_deps.extend(c.dev_path_deps);
            draft.crate_name = Some(c.name);
        }
        let packages = pnpm::discover(&root, &files)?;
        let package_roots: BTreeMap<String, PathBuf> = packages
            .iter()
            .map(|p| (p.name.clone(), p.root.clone()))
            .collect();
        for p in packages {
            let draft = drafts.entry(p.root).or_default();
            draft.inferred.extend(pnpm::targets(&p.scripts));
            draft.deps.extend(
                p.workspace_deps
                    .iter()
                    .filter_map(|name| package_roots.get(name).cloned()),
            );
            draft.js_name = Some(p.name);
        }
        for path in declared.keys() {
            drafts.entry(path.clone()).or_insert_with(|| Draft {
                explicit: true,
                ..Draft::default()
            });
        }

        let names = name_projects(&drafts, &declared)?;
        let roots_by_name: BTreeMap<&str, &PathBuf> = names
            .iter()
            .map(|(root, name)| (name.as_str(), root))
            .collect();
        for (path, project) in &declared {
            let deps = project
                .deps
                .iter()
                .map(|dep| resolve_dep(dep, path, &names, &roots_by_name))
                .collect::<Result<Vec<_>>>()?;
            drafts
                .get_mut(path)
                .expect("declared projects have drafts")
                .deps
                .extend(deps);
        }

        let owners = Owners::new(drafts.keys().cloned());
        let mut owned: BTreeMap<PathBuf, Vec<PathBuf>> =
            drafts.keys().map(|r| (r.clone(), Vec::new())).collect();
        for file in &files {
            if let Some(owner) = owners.owner(file) {
                owned
                    .get_mut(owner)
                    .expect("owners are project roots")
                    .push(file.clone());
            }
        }
        let ts_paths = ts::TsPaths::load(&root)?;
        for (project, edges) in ts::import_edges(&root, &owned, &owners, &package_roots, &ts_paths)
        {
            let draft = drafts
                .get_mut(&project)
                .expect("edges start at project roots");
            draft.deps.extend(edges.deps);
            draft.dev_deps.extend(edges.dev_deps);
        }

        let projects = drafts
            .into_iter()
            .map(|(path, draft)| {
                let name = names[&path].clone();
                let overrides = declared.get(&path).map(|p| &p.targets);
                let project = Project {
                    targets: resolve_targets(&name, &draft.inferred, overrides, &config.targets)?,
                    deps: draft
                        .deps
                        .iter()
                        .filter(|d| **d != path)
                        .map(|d| names[d].clone())
                        .collect(),
                    dev_deps: draft
                        .dev_deps
                        .iter()
                        .filter(|d| **d != path && !draft.deps.contains(*d))
                        .map(|d| names[d].clone())
                        .collect(),
                    kinds: draft.kinds(),
                    crate_name: draft.crate_name,
                    files: owned.remove(&path).unwrap_or_default(),
                    root: path,
                    name: name.clone(),
                };
                Ok::<_, Error>((name, project))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;

        Ok(Workspace {
            root,
            config,
            projects,
            files,
        })
    }

    /// Every target name any project has, sorted.
    pub fn target_names(&self) -> Vec<String> {
        self.projects
            .values()
            .flat_map(|p| p.targets.keys().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn check_projects(&self, names: &BTreeSet<String>) -> Result<()> {
        names
            .iter()
            .find(|n| !self.projects.contains_key(*n))
            .map_or(Ok(()), |n| Err(Error::UnknownProject(n.clone())))
    }

    /// Projects whose changes can affect `name`: its deps and dev-deps, then their deps
    /// transitively (a dependency's dev-deps don't count). Never includes `name` itself.
    pub fn dependency_closure(&self, name: &str) -> BTreeSet<&str> {
        let project = &self.projects[name];
        let mut seen = BTreeSet::new();
        let mut stack: Vec<&str> = project
            .deps
            .iter()
            .chain(&project.dev_deps)
            .map(String::as_str)
            .collect();
        while let Some(dep) = stack.pop() {
            if dep != name && seen.insert(dep) {
                stack.extend(self.projects[dep].deps.iter().map(String::as_str));
            }
        }
        seen
    }
}

fn project_path(key: &str) -> Result<PathBuf> {
    files::normalize(Path::new(key)).ok_or_else(|| Error::Config {
        path: PathBuf::from(config::FILE),
        message: format!("project path `{key}` must be relative and inside the workspace"),
    })
}

/// `[projects]` entries by normalised path; two keys for one path are an error.
fn declared_projects(config: &Config) -> Result<BTreeMap<PathBuf, &ProjectConfig>> {
    let mut keys: BTreeMap<PathBuf, &str> = BTreeMap::new();
    config
        .projects
        .iter()
        .map(|(key, project)| {
            let path = project_path(key)?;
            match keys.insert(path.clone(), key) {
                Some(first) => Err(Error::Config {
                    path: PathBuf::from(config::FILE),
                    message: format!(
                        "projects `{first}` and `{key}` are the same path `{}`",
                        display_root(&path)
                    ),
                }),
                None => Ok((path, project)),
            }
        })
        .collect()
}

/// The root of the project an explicit `deps` entry names, by path or by final name.
fn resolve_dep(
    dep: &str,
    owner: &Path,
    names: &BTreeMap<PathBuf, String>,
    roots_by_name: &BTreeMap<&str, &PathBuf>,
) -> Result<PathBuf> {
    let by_path = project_path(dep).ok().filter(|p| names.contains_key(p));
    let by_name = roots_by_name.get(dep).map(|r| (*r).clone());
    match (by_path, by_name) {
        (Some(path), Some(named)) if path != named => Err(Error::Config {
            path: PathBuf::from(config::FILE),
            message: format!(
                "dep `{dep}` of `{}` is ambiguous: it is the path of `{}` and the name of the project at `{}`",
                display_root(owner),
                names[&path],
                display_root(&named)
            ),
        }),
        (by_path, by_name) => by_path
            .or(by_name)
            .ok_or_else(|| Error::UnknownProject(dep.into())),
    }
}

/// A declared name wins, then the package name, the crate name and the path.
fn name_projects(
    drafts: &BTreeMap<PathBuf, Draft>,
    declared: &BTreeMap<PathBuf, &ProjectConfig>,
) -> Result<BTreeMap<PathBuf, String>> {
    let mut seen: BTreeMap<String, &Path> = BTreeMap::new();
    drafts
        .iter()
        .map(|(root, draft)| {
            let name = declared
                .get(root)
                .and_then(|p| p.name.clone())
                .into_iter()
                .chain(draft.js_name.clone())
                .chain(draft.crate_name.clone())
                .find(|n| !n.is_empty())
                .unwrap_or_else(|| display_root(root));
            if let Some(first) = seen.insert(name.clone(), root) {
                return Err(Error::DuplicateProject {
                    name,
                    first: display_root(first),
                    second: display_root(root),
                });
            }
            Ok((root.clone(), name))
        })
        .collect()
}

/// Explicit project fields, then inferred ones, then `[targets.<name>]` defaults.
fn resolve_targets(
    project: &str,
    inferred: &BTreeMap<String, TargetConfig>,
    overrides: Option<&BTreeMap<String, TargetConfig>>,
    defaults: &BTreeMap<String, TargetConfig>,
) -> Result<BTreeMap<String, Target>> {
    let none = BTreeMap::new();
    let overrides = overrides.unwrap_or(&none);
    inferred
        .keys()
        .chain(overrides.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|name| {
            let merged = overrides
                .get(name)
                .cloned()
                .unwrap_or_default()
                .or(&inferred.get(name).cloned().unwrap_or_default())
                .or(&defaults.get(name).cloned().unwrap_or_default());
            Target::finalize(project, name, merged).map(|t| (name.clone(), t))
        })
        .collect()
}

pub fn to_dot(ws: &Workspace) -> String {
    let body: String = ws
        .projects
        .values()
        .map(|p| {
            std::iter::once(format!("  {:?};\n", p.name))
                .chain(
                    p.deps
                        .iter()
                        .map(|d| format!("  {:?} -> {:?};\n", p.name, d)),
                )
                .collect::<String>()
        })
        .collect();
    format!("digraph axonal {{\n{body}}}\n")
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A project rooted at `name` whose targets run `echo <target>` over all its files.
    pub fn project(name: &str, deps: &[&str], targets: &[(&str, &[&str])]) -> Project {
        Project {
            name: name.into(),
            root: PathBuf::from(name),
            kinds: BTreeSet::from([Kind::Explicit]),
            deps: deps.iter().map(|d| d.to_string()).collect(),
            dev_deps: BTreeSet::new(),
            crate_name: None,
            files: vec![],
            targets: targets
                .iter()
                .map(|(t, depends_on)| {
                    let target = Target {
                        command: format!("echo {t}"),
                        depends_on: depends_on.iter().map(|d| d.to_string()).collect(),
                        inputs: vec!["**/*".into()],
                        outputs: vec![],
                        env: vec![],
                        deps_usage: DepsUsage::Impl,
                        persistent: false,
                    };
                    (t.to_string(), target)
                })
                .collect(),
        }
    }

    pub fn workspace(projects: Vec<Project>) -> Workspace {
        Workspace {
            root: PathBuf::from("/ws"),
            config: Config::default(),
            files: vec![],
            projects: projects.into_iter().map(|p| (p.name.clone(), p)).collect(),
        }
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

    #[test]
    fn explicit_projects_get_defaults_and_overrides() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "axonal.toml",
            r#"
[targets.build]
depends_on = ["^build"]
outputs = ["dist/**"]

[projects."libs/a".targets.build]
command = "make"

[projects."apps/b"]
deps = ["libs/a"]

[projects."apps/b".targets.build]
command = "make b"
outputs = []
"#,
        );
        write(dir.path(), "libs/a/src/a.txt", "a");
        write(dir.path(), "apps/b/src/b.txt", "b");
        let ws = Workspace::discover(dir.path()).unwrap();
        let a = &ws.projects["libs/a"];
        assert_eq!(a.kinds, BTreeSet::from([Kind::Explicit]));
        assert_eq!(
            a.targets["build"],
            Target {
                command: "make".into(),
                depends_on: vec!["^build".into()],
                inputs: vec!["**/*".into()],
                outputs: vec!["dist/**".into()],
                env: vec![],
                deps_usage: DepsUsage::Impl,
                persistent: false,
            }
        );
        assert_eq!(a.files, vec![PathBuf::from("libs/a/src/a.txt")]);
        let b = &ws.projects["apps/b"];
        assert_eq!(b.deps, BTreeSet::from(["libs/a".to_string()]));
        assert!(b.targets["build"].outputs.is_empty());
    }

    #[test]
    fn pnpm_and_cargo_projects_at_one_root_merge() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "pnpm-workspace.yaml",
            "packages: ['crates/*']\n",
        );
        write(
            dir.path(),
            "crates/napi/package.json",
            r#"{"name":"@x/napi","scripts":{"build":"napi build"}}"#,
        );
        write(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
        );
        write(
            dir.path(),
            "crates/napi/Cargo.toml",
            "[package]\nname = \"napi-core\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        write(dir.path(), "crates/napi/src/lib.rs", "");
        let ws = Workspace::discover(dir.path()).unwrap();
        let p = &ws.projects["@x/napi"];
        assert_eq!(p.kinds, BTreeSet::from([Kind::Js, Kind::Cargo]));
        assert_eq!(p.crate_name.as_deref(), Some("napi-core"));
        assert_eq!(p.targets["build"].command, "pnpm run build");
        assert_eq!(p.targets["test"].command, "cargo test -p napi-core");
    }

    #[test]
    fn deps_accept_names_and_reject_unknown_projects() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pnpm-workspace.yaml", "packages: ['libs/*']\n");
        write(dir.path(), "libs/ui/package.json", r#"{"name":"@acme/ui"}"#);
        write(
            dir.path(),
            "axonal.toml",
            "[projects.\"apps/x\"]\ndeps = [\"@acme/ui\"]\n",
        );
        let ws = Workspace::discover(dir.path()).unwrap();
        assert_eq!(
            ws.projects["apps/x"].deps,
            BTreeSet::from(["@acme/ui".to_string()])
        );

        write(
            dir.path(),
            "axonal.toml",
            "[projects.\"apps/x\"]\ndeps = [\"ghost\"]\n",
        );
        assert!(matches!(
            Workspace::discover(dir.path()),
            Err(Error::UnknownProject(name)) if name == "ghost"
        ));
    }

    #[test]
    fn duplicate_project_paths_are_a_config_error() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "axonal.toml",
            "[projects.\"libs/a\"]\n\n[projects.\"./libs/a/\"]\n",
        );
        match Workspace::discover(dir.path()) {
            Err(Error::Config { message, .. }) => {
                assert!(message.contains("`libs/a`"), "{message}");
                assert!(message.contains("`./libs/a/`"), "{message}");
            }
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    #[test]
    fn unnamed_root_package_is_named_dot() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pnpm-workspace.yaml", "packages: ['.']\n");
        for manifest in ["{}", r#"{"name":""}"#] {
            write(dir.path(), "package.json", manifest);
            let ws = Workspace::discover(dir.path()).unwrap();
            assert_eq!(ws.projects.keys().collect::<Vec<_>>(), ["."], "{manifest}");
        }
    }

    #[test]
    fn declared_names_resolve_collisions_and_name_deps() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
        );
        write(
            dir.path(),
            "crates/core/Cargo.toml",
            "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        write(dir.path(), "crates/core/src/lib.rs", "");
        write(
            dir.path(),
            "pnpm-workspace.yaml",
            "packages: ['packages/*']\n",
        );
        write(
            dir.path(),
            "packages/core/package.json",
            r#"{"name":"core"}"#,
        );
        assert!(matches!(
            Workspace::discover(dir.path()),
            Err(Error::DuplicateProject { name, .. }) if name == "core"
        ));

        write(
            dir.path(),
            "axonal.toml",
            "[projects.\"crates/core\"]\nname = \"core-rs\"\n\n[projects.\"packages/core\"]\ndeps = [\"core-rs\"]\n",
        );
        let ws = Workspace::discover(dir.path()).unwrap();
        let rs = &ws.projects["core-rs"];
        assert_eq!(rs.crate_name.as_deref(), Some("core"));
        assert_eq!(rs.kinds, BTreeSet::from([Kind::Cargo]));
        assert_eq!(rs.targets["test"].command, "cargo test -p core");
        assert_eq!(
            ws.projects["core"].deps,
            BTreeSet::from(["core-rs".to_string()])
        );
    }

    #[test]
    fn deps_naming_one_project_by_path_and_another_by_name_are_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pnpm-workspace.yaml", "packages: ['ui']\n");
        write(dir.path(), "ui/package.json", r#"{"name":"@acme/ui"}"#);
        write(
            dir.path(),
            "axonal.toml",
            "[projects.tools]\nname = \"ui\"\n\n[projects.app]\ndeps = [\"ui\"]\n",
        );
        match Workspace::discover(dir.path()) {
            Err(Error::Config { message, .. }) => {
                assert!(message.contains("ambiguous"), "{message}");
                assert!(message.contains("@acme/ui"), "{message}");
                assert!(message.contains("tools"), "{message}");
            }
            other => panic!("expected a config error, got {other:?}"),
        }

        write(
            dir.path(),
            "axonal.toml",
            "[projects.ui]\nname = \"ui\"\n\n[projects.app]\ndeps = [\"ui\"]\n",
        );
        let ws = Workspace::discover(dir.path()).unwrap();
        assert_eq!(ws.projects["app"].deps, BTreeSet::from(["ui".to_string()]));
    }

    #[test]
    fn explicit_config_merges_into_an_inferred_project() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pnpm-workspace.yaml", "packages: ['libs/*']\n");
        write(
            dir.path(),
            "libs/ui/package.json",
            r#"{"name":"@acme/ui","scripts":{"build":"tsc"}}"#,
        );
        write(
            dir.path(),
            "axonal.toml",
            r#"
[projects."libs/ui"]
deps = ["tools"]

[projects."libs/ui".targets.build]
outputs = ["dist/**"]

[projects."libs/ui".targets.lint]
command = "eslint ."

[projects.tools.targets.build]
command = "true"
"#,
        );
        let ws = Workspace::discover(dir.path()).unwrap();
        let ui = &ws.projects["@acme/ui"];
        assert_eq!(ui.kinds, BTreeSet::from([Kind::Js]));
        assert_eq!(ui.deps, BTreeSet::from(["tools".to_string()]));
        assert_eq!(ui.targets["build"].command, "pnpm run build");
        assert_eq!(ui.targets["build"].outputs, vec!["dist/**".to_string()]);
        assert_eq!(ui.targets["lint"].command, "eslint .");
    }

    #[test]
    fn a_target_without_a_command_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "axonal.toml",
            "[projects.\"libs/a\".targets.build]\noutputs = [\"dist/**\"]\n",
        );
        assert!(matches!(
            Workspace::discover(dir.path()),
            Err(Error::MissingCommand { .. })
        ));
    }

    #[test]
    fn duplicate_names_are_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pnpm-workspace.yaml", "packages: ['apps/*']\n");
        write(dir.path(), "apps/one/package.json", r#"{"name":"dup"}"#);
        write(dir.path(), "apps/two/package.json", r#"{"name":"dup"}"#);
        assert!(matches!(
            Workspace::discover(dir.path()),
            Err(Error::DuplicateProject { .. })
        ));
    }

    #[test]
    fn ts_imports_add_edges() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "tsconfig.base.json",
            r#"{ "compilerOptions": { "paths": { "@acme/money": ["libs/money/src/index.ts"] } } }"#,
        );
        write(
            dir.path(),
            "axonal.toml",
            "[projects.\"libs/money\".targets.test]\ncommand = \"true\"\n\n[projects.\"libs/billing\".targets.test]\ncommand = \"true\"\n",
        );
        write(
            dir.path(),
            "libs/money/src/index.ts",
            "export const cents = 1;\n",
        );
        write(
            dir.path(),
            "libs/billing/src/index.ts",
            "import { cents } from '@acme/money';\n",
        );
        let ws = Workspace::discover(dir.path()).unwrap();
        assert_eq!(
            ws.projects["libs/billing"].deps,
            BTreeSet::from(["libs/money".to_string()])
        );
        assert!(ws.projects["libs/money"].deps.is_empty());
    }

    #[test]
    fn test_file_imports_are_dev_edges_and_break_cycles() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pnpm-workspace.yaml", "packages: ['libs/*']\n");
        write(dir.path(), "libs/a/package.json", r#"{"name":"@acme/a"}"#);
        write(dir.path(), "libs/b/package.json", r#"{"name":"@acme/b"}"#);
        write(dir.path(), "libs/a/src/a.ts", "export const a = 1;\n");
        write(
            dir.path(),
            "libs/a/src/a.test.ts",
            "import { b } from '@acme/b';\n",
        );
        write(
            dir.path(),
            "libs/b/src/b.ts",
            "import { a } from '@acme/a';\n",
        );
        let ws = Workspace::discover(dir.path()).unwrap();
        let a = &ws.projects["@acme/a"];
        let b = &ws.projects["@acme/b"];
        assert!(a.deps.is_empty());
        assert_eq!(a.dev_deps, BTreeSet::from(["@acme/b".to_string()]));
        assert_eq!(b.deps, BTreeSet::from(["@acme/a".to_string()]));
        assert!(b.dev_deps.is_empty());
    }

    #[test]
    fn nested_projects_own_their_files() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "axonal.toml",
            "[projects.\".\".targets.check]\ncommand = \"true\"\n\n[projects.\"libs/a\".targets.check]\ncommand = \"true\"\n",
        );
        write(dir.path(), "README.md", "hi");
        write(dir.path(), "libs/a/x.txt", "x");
        let ws = Workspace::discover(dir.path()).unwrap();
        assert_eq!(
            ws.projects["."].files,
            vec![PathBuf::from("README.md"), PathBuf::from("axonal.toml")]
        );
        assert_eq!(
            ws.projects["libs/a"].files,
            vec![PathBuf::from("libs/a/x.txt")]
        );
    }

    #[test]
    fn dot_lists_every_edge() {
        let ws = testing::workspace(vec![
            testing::project("app", &["lib"], &[]),
            testing::project("lib", &[], &[]),
        ]);
        assert_eq!(
            to_dot(&ws),
            "digraph axonal {\n  \"app\";\n  \"app\" -> \"lib\";\n  \"lib\";\n}\n"
        );
    }

    #[test]
    fn dependency_closure_follows_dev_edges_one_hop_and_tolerates_cycles() {
        let mut core = testing::project("core", &[], &[]);
        core.dev_deps = BTreeSet::from(["test-utils".to_string()]);
        let mut test_utils = testing::project("test-utils", &["core"], &[]);
        test_utils.dev_deps = BTreeSet::from(["fixtures".to_string()]);
        let ws = testing::workspace(vec![
            testing::project("app", &["core"], &[]),
            core,
            testing::project("fixtures", &[], &[]),
            test_utils,
        ]);
        assert_eq!(ws.dependency_closure("app"), BTreeSet::from(["core"]));
        assert_eq!(
            ws.dependency_closure("core"),
            BTreeSet::from(["test-utils"])
        );
        assert_eq!(
            ws.dependency_closure("test-utils"),
            BTreeSet::from(["core", "fixtures"])
        );
    }
}
