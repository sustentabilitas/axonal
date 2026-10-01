//! Affected tasks: changed files and lockfile diffs since a base revision, propagated
//! through project dependencies and the task graph. This is the upper bound pruning shrinks.

pub mod lockfile;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::{
    config,
    error::Result,
    files::{Owners, Patterns},
    git,
    graph::{Project, TaskGraph, TaskId, Workspace},
    hash::ImplicitInputs,
};
use lockfile::{CARGO_LOCK, PNPM_LOCK};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Cause {
    /// Files matching the task's own inputs changed.
    Files { files: Vec<PathBuf> },
    /// The project's resolved external dependencies changed.
    Lockfile,
    /// `axonal.toml`, or a file every task of the project hashes whatever its `inputs`:
    /// workspace and project manifests, toolchain files, tool configs, `[workspace] inputs`.
    Manifest { file: PathBuf },
    /// A project this one depends on, directly or transitively, changed.
    Dependency { project: String },
    /// A task this one depends on is affected.
    Upstream { task: TaskId },
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Affected {
    pub files: BTreeSet<PathBuf>,
    /// Changed projects plus every project depending on one.
    pub projects: BTreeSet<String>,
    pub tasks: BTreeMap<TaskId, Vec<Cause>>,
}

#[derive(Debug, Clone)]
pub struct Range {
    pub base: String,
    /// `None` compares against the working tree.
    pub head: Option<String>,
}

impl Range {
    /// The base defaults to the merge-base of the default branch and the head.
    pub fn resolve(ws: &Workspace, base: Option<&str>, head: Option<&str>) -> Result<Range> {
        let base = match base {
            Some(base) => base.to_string(),
            None => git::default_base(
                &ws.root,
                &ws.config.workspace.default_branch,
                head.unwrap_or("HEAD"),
            )?,
        };
        Ok(Range {
            base,
            head: head.map(str::to_string),
        })
    }
}

type Versions = Option<(Option<String>, Option<String>)>;

pub fn from_git(ws: &Workspace, graph: &TaskGraph, range: &Range) -> Result<Affected> {
    let files = git::changed_files(&ws.root, &range.base, range.head.as_deref())?;
    let versions = |name: &str| -> Result<Versions> {
        if !files.contains(Path::new(name)) {
            return Ok(None);
        }
        let old = git::show(&ws.root, &range.base, Path::new(name))?;
        let new = match &range.head {
            Some(head) => git::show(&ws.root, head, Path::new(name))?,
            None => std::fs::read_to_string(ws.root.join(name)).ok(),
        };
        Ok(Some((old, new)))
    };
    let (pnpm_lock, cargo_lock) = (versions(PNPM_LOCK)?, versions(CARGO_LOCK)?);
    let impacted = lockfile::impacted(ws, as_refs(&pnpm_lock), as_refs(&cargo_lock));
    compute(ws, graph, &files, &impacted)
}

fn as_refs(versions: &Versions) -> Option<(Option<&str>, Option<&str>)> {
    versions
        .as_ref()
        .map(|(old, new)| (old.as_deref(), new.as_deref()))
}

pub fn compute(
    ws: &Workspace,
    graph: &TaskGraph,
    files: &BTreeSet<PathBuf>,
    lock_impacted: &BTreeSet<String>,
) -> Result<Affected> {
    let changes = Changes::new(ws, files)?;
    let base_causes: BTreeMap<&str, Vec<Cause>> = ws
        .projects
        .values()
        .map(|p| {
            let causes = lock_impacted
                .contains(&p.name)
                .then_some(Cause::Lockfile)
                .into_iter()
                .chain(
                    changes
                        .manifests(p)
                        .map(|f| Cause::Manifest { file: f.clone() }),
                )
                .collect();
            (p.name.as_str(), causes)
        })
        .collect();
    let changed: BTreeSet<&str> = ws
        .projects
        .values()
        .filter(|p| {
            !base_causes[p.name.as_str()].is_empty()
                || if p.targets.is_empty() {
                    !changes.owned(p).is_empty()
                } else {
                    p.targets.keys().any(|t| !changes.own_hits(p, t).is_empty())
                }
        })
        .map(|p| p.name.as_str())
        .collect();
    let upstream: BTreeMap<&str, BTreeSet<&str>> = ws
        .projects
        .keys()
        .map(|name| (name.as_str(), ws.dependency_closure(name)))
        .collect();

    let mut tasks: BTreeMap<TaskId, Vec<Cause>> = BTreeMap::new();
    for id in &graph.order {
        let p = &ws.projects[&id.project];
        let hits: Vec<PathBuf> = changes
            .own_hits(p, &id.target)
            .into_iter()
            .chain(changes.workspace_hits(p, &id.target))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut causes: Vec<Cause> = (!hits.is_empty())
            .then_some(Cause::Files { files: hits })
            .into_iter()
            .collect();
        causes.extend(base_causes[p.name.as_str()].iter().cloned());
        let patterns = changes.patterns(p, &id.target);
        causes.extend(
            upstream[p.name.as_str()]
                .iter()
                .filter(|d| {
                    changed.contains(*d)
                        || !changes.matching(&ws.projects[**d], patterns).is_empty()
                })
                .map(|d| Cause::Dependency {
                    project: d.to_string(),
                }),
        );
        causes.extend(
            graph.deps[id]
                .iter()
                .filter(|d| tasks.contains_key(*d))
                .map(|d| Cause::Upstream { task: d.clone() }),
        );
        if !causes.is_empty() {
            tasks.insert(id.clone(), causes);
        }
    }

    let projects = ws
        .projects
        .keys()
        .filter(|name| {
            changed.contains(name.as_str())
                || upstream[name.as_str()].iter().any(|d| changed.contains(d))
        })
        .cloned()
        .chain(tasks.keys().map(|id| id.project.clone()))
        .collect();
    Ok(Affected {
        files: files.clone(),
        projects,
        tasks,
    })
}

/// Changed files other than root lockfiles, grouped by owning project, with each target's
/// compiled input patterns.
struct Changes<'a> {
    files: Vec<&'a PathBuf>,
    owned: BTreeMap<PathBuf, Vec<&'a PathBuf>>,
    patterns: BTreeMap<String, BTreeMap<String, Patterns>>,
    implicit: ImplicitInputs,
}

impl<'a> Changes<'a> {
    fn new(ws: &Workspace, files: &'a BTreeSet<PathBuf>) -> Result<Self> {
        let patterns = ws
            .projects
            .values()
            .map(|p| {
                p.targets
                    .iter()
                    .map(|(name, t)| Patterns::new(&t.inputs).map(|pats| (name.clone(), pats)))
                    .collect::<Result<BTreeMap<_, _>>>()
                    .map(|targets| (p.name.clone(), targets))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let files: Vec<&PathBuf> = files.iter().filter(|f| !is_lockfile(f)).collect();
        let owners = Owners::new(ws.projects.values().map(|p| p.root.clone()));
        let owned = files.iter().fold(
            BTreeMap::new(),
            |mut owned: BTreeMap<PathBuf, Vec<_>>, f| {
                if let Some(root) = owners.owner(f) {
                    owned.entry(root.to_path_buf()).or_default().push(*f);
                }
                owned
            },
        );
        Ok(Changes {
            files,
            owned,
            patterns,
            implicit: ImplicitInputs::new(ws)?,
        })
    }

    fn owned(&self, p: &Project) -> &[&'a PathBuf] {
        self.owned.get(&p.root).map_or(&[], Vec::as_slice)
    }

    fn patterns(&self, p: &Project, target: &str) -> &Patterns {
        &self.patterns[&p.name][target]
    }

    fn own_hits(&self, p: &Project, target: &str) -> Vec<PathBuf> {
        self.matching(p, self.patterns(p, target))
    }

    /// Changed files `owner` owns matching the project-relative `patterns`, which may be
    /// another project's: dependents hash their dependencies' files through their own inputs.
    fn matching(&self, owner: &Project, patterns: &Patterns) -> Vec<PathBuf> {
        self.owned(owner)
            .iter()
            .filter(|f| {
                f.strip_prefix(&owner.root)
                    .is_ok_and(|rel| patterns.matches_project(rel))
            })
            .map(|f| (*f).clone())
            .collect()
    }

    fn workspace_hits(&self, p: &Project, target: &str) -> Vec<PathBuf> {
        let patterns = self.patterns(p, target);
        self.files
            .iter()
            .filter(|f| patterns.matches_workspace(f))
            .map(|f| (*f).clone())
            .collect()
    }

    fn manifests<'s>(&'s self, p: &'s Project) -> impl Iterator<Item = &'a PathBuf> + 's {
        self.files
            .iter()
            .copied()
            .filter(|f| f.as_path() == Path::new(config::FILE) || self.implicit.applies(p, f))
    }
}

fn is_lockfile(path: &Path) -> bool {
    path == Path::new(PNPM_LOCK) || path == Path::new(CARGO_LOCK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{
        Kind,
        testing::{project, workspace},
    };

    fn ws() -> Workspace {
        let mut lib = project("lib", &["types"], &[("build", &["^build"]), ("lint", &[])]);
        let mut app = project("app", &["lib"], &[("build", &["^build"]), ("lint", &[])]);
        let tool = project("tool", &[], &[("build", &[]), ("lint", &[])]);
        let types = project("types", &[], &[]);
        for p in [&mut lib, &mut app] {
            p.targets.get_mut("build").unwrap().inputs = vec!["src/**".into()];
            p.targets.get_mut("lint").unwrap().inputs =
                vec!["src/**".into(), "{workspace}/lint-rules.json".into()];
        }
        workspace(vec![lib, app, tool, types])
    }

    fn affected(ws: &Workspace, files: &[&str], impacted: &[&str]) -> Affected {
        let graph = TaskGraph::build(ws, &["build".into(), "lint".into()], None).unwrap();
        compute(
            ws,
            &graph,
            &files.iter().map(PathBuf::from).collect(),
            &impacted.iter().map(|s| s.to_string()).collect(),
        )
        .unwrap()
    }

    fn ids(a: &Affected) -> Vec<String> {
        a.tasks.keys().map(ToString::to_string).collect()
    }

    #[test]
    fn own_changes_affect_the_project_and_its_dependents() {
        let a = affected(&ws(), &["lib/src/a.ts"], &[]);
        assert_eq!(ids(&a), ["app:build", "app:lint", "lib:build", "lib:lint"]);
        assert_eq!(
            a.tasks[&TaskId::new("lib", "build")],
            vec![Cause::Files {
                files: vec!["lib/src/a.ts".into()]
            }]
        );
        assert_eq!(
            a.tasks[&TaskId::new("app", "build")],
            vec![
                Cause::Dependency {
                    project: "lib".into()
                },
                Cause::Upstream {
                    task: TaskId::new("lib", "build")
                },
            ]
        );
        assert_eq!(
            a.projects,
            BTreeSet::from(["app".to_string(), "lib".to_string()])
        );
    }

    #[test]
    fn files_outside_inputs_affect_nothing() {
        let a = affected(&ws(), &["lib/README.md", "README.md"], &[]);
        assert!(a.tasks.is_empty());
        assert!(a.projects.is_empty());
    }

    #[test]
    fn workspace_globs_affect_only_matching_tasks() {
        assert_eq!(
            ids(&affected(&ws(), &["lint-rules.json"], &[])),
            ["app:lint", "lib:lint"]
        );
    }

    #[test]
    fn projects_without_targets_propagate_their_changes() {
        let a = affected(&ws(), &["types/index.ts"], &[]);
        assert_eq!(ids(&a), ["app:build", "app:lint", "lib:build", "lib:lint"]);
        assert!(a.projects.contains("types"));
    }

    #[test]
    fn lockfile_impact_marks_projects_changed_and_lockfiles_never_match_globs() {
        let mut ws = ws();
        ws.projects
            .get_mut("tool")
            .unwrap()
            .targets
            .get_mut("build")
            .unwrap()
            .inputs = vec!["{workspace}/pnpm-lock.yaml".into()];
        let a = affected(&ws, &["pnpm-lock.yaml"], &["lib"]);
        assert_eq!(a.tasks[&TaskId::new("lib", "build")], vec![Cause::Lockfile]);
        assert!(a.tasks.contains_key(&TaskId::new("app", "build")));
        assert!(!a.tasks.contains_key(&TaskId::new("tool", "build")));
    }

    #[test]
    fn manifests_affect_their_ecosystem() {
        let mut ws = ws();
        for p in ws.projects.values_mut() {
            p.kinds = BTreeSet::from([if p.name == "tool" {
                Kind::Cargo
            } else {
                Kind::Js
            }]);
        }
        assert_eq!(
            ids(&affected(&ws, &["Cargo.toml"], &[])),
            ["tool:build", "tool:lint"]
        );
        assert_eq!(
            ids(&affected(&ws, &["rust-toolchain.toml"], &[])),
            ["tool:build", "tool:lint"]
        );
        let js = ["app:build", "app:lint", "lib:build", "lib:lint"];
        assert_eq!(ids(&affected(&ws, &["pnpm-workspace.yaml"], &[])), js);
        assert_eq!(ids(&affected(&ws, &[".eslintrc.json"], &[])), js);
        assert_eq!(affected(&ws, &["axonal.toml"], &[]).tasks.len(), 6);
    }

    #[test]
    fn project_manifests_count_whatever_the_inputs() {
        let a = affected(&ws(), &["lib/package.json"], &[]);
        assert_eq!(ids(&a), ["app:build", "app:lint", "lib:build", "lib:lint"]);
        assert_eq!(
            a.tasks[&TaskId::new("lib", "lint")],
            vec![Cause::Manifest {
                file: "lib/package.json".into()
            }]
        );
    }

    #[test]
    fn dependents_see_dependency_files_through_their_own_inputs() {
        let mut dep = project("d", &[], &[("build", &[])]);
        dep.targets.get_mut("build").unwrap().inputs = vec!["src/**".into()];
        let app = project("app", &["d"], &[("test", &[])]);
        let ws = workspace(vec![dep, app]);
        let graph = TaskGraph::build(&ws, &["build".into(), "test".into()], None).unwrap();
        let files = BTreeSet::from([PathBuf::from("d/assets/data.json")]);
        let a = compute(&ws, &graph, &files, &BTreeSet::new()).unwrap();
        assert_eq!(ids(&a), ["app:test"]);
        assert_eq!(
            a.tasks[&TaskId::new("app", "test")],
            vec![Cause::Dependency {
                project: "d".into()
            }]
        );
    }

    #[test]
    fn explicit_projects_see_every_ecosystem() {
        let a = affected(&ws(), &["Cargo.toml"], &[]);
        assert_eq!(a.tasks.len(), 6);
    }
}
