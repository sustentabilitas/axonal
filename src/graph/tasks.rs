//! The task graph: `project:target` tasks and the edges `depends_on` implies.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fmt,
};

use serde::{Serialize, Serializer};

use super::{Project, Workspace};
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId {
    pub project: String,
    pub target: String,
}

impl TaskId {
    pub fn new(project: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            project: project.into(),
            target: target.into(),
        }
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.project, self.target)
    }
}

impl Serialize for TaskId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[derive(Debug, Clone, Default)]
pub struct TaskGraph {
    pub deps: BTreeMap<TaskId, BTreeSet<TaskId>>,
    /// Every task, dependencies before dependents.
    pub order: Vec<TaskId>,
}

impl TaskGraph {
    /// `targets` in `projects` (every project when `None`), plus everything they depend on.
    pub fn build(
        ws: &Workspace,
        targets: &[String],
        projects: Option<&BTreeSet<String>>,
    ) -> Result<TaskGraph> {
        if let Some(names) = projects {
            ws.check_projects(names)?;
        }
        let selected: Vec<&Project> = ws
            .projects
            .values()
            .filter(|p| projects.is_none_or(|names| names.contains(&p.name)))
            .collect();
        if let Some(missing) = targets
            .iter()
            .find(|t| !selected.iter().any(|p| p.targets.contains_key(*t)))
        {
            return Err(Error::UnknownTarget(missing.clone()));
        }
        let roots = selected
            .iter()
            .flat_map(|p| {
                targets
                    .iter()
                    .filter(|t| p.targets.contains_key(*t))
                    .map(|t| TaskId::new(&p.name, t))
            })
            .collect();
        Self::from_roots(ws, roots)
    }

    pub fn from_roots(ws: &Workspace, roots: Vec<TaskId>) -> Result<TaskGraph> {
        if let Some(unknown) = roots.iter().find(|id| ws.get_target(id).is_none()) {
            return Err(Error::UnknownTask(unknown.to_string()));
        }
        let mut upstream_cache = UpstreamCache::new();
        let mut deps = BTreeMap::new();
        let mut stack = roots;
        while let Some(id) = stack.pop() {
            if deps.contains_key(&id) {
                continue;
            }
            let task_deps = task_deps(ws, &id, &mut upstream_cache);
            stack.extend(task_deps.iter().cloned());
            deps.insert(id, task_deps);
        }
        check_persistent(ws, &deps)?;
        let order = topo_order(&deps)?;
        Ok(TaskGraph { deps, order })
    }

    pub fn dependents(&self) -> BTreeMap<TaskId, BTreeSet<TaskId>> {
        invert(&self.deps)
    }
}

/// `^target` results by `(project, target)`, shared by every task in one graph.
type UpstreamCache<'a> = HashMap<(&'a str, &'a str), BTreeSet<TaskId>>;

fn task_deps<'a>(
    ws: &'a Workspace,
    id: &TaskId,
    upstream_cache: &mut UpstreamCache<'a>,
) -> BTreeSet<TaskId> {
    let project = &ws.projects[&id.project];
    project.targets[&id.target]
        .depends_on
        .iter()
        .flat_map(|dep| match dep.strip_prefix('^') {
            Some(target) => upstream_cache
                .entry((project.name.as_str(), target))
                .or_insert_with(|| upstream(ws, project, target))
                .clone(),
            None => project
                .targets
                .contains_key(dep.as_str())
                .then(|| TaskId::new(&project.name, dep))
                .into_iter()
                .collect(),
        })
        .collect()
}

/// `^target`: the target in each dependency, looking through dependencies that lack it.
fn upstream(ws: &Workspace, project: &Project, target: &str) -> BTreeSet<TaskId> {
    #[cfg(test)]
    tests::record_upstream_lookup(&project.name, target);
    let mut found = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut stack: Vec<&str> = project.deps.iter().map(String::as_str).collect();
    while let Some(name) = stack.pop() {
        if !seen.insert(name) {
            continue;
        }
        // `deps` names real projects, as `Workspace::discover` guarantees.
        let dep = &ws.projects[name];
        if dep.targets.contains_key(target) {
            found.insert(TaskId::new(name, target));
        } else {
            stack.extend(dep.deps.iter().map(String::as_str));
        }
    }
    found
}

/// `run` would wait forever on a dependency that never finishes; persistent roots are fine.
fn check_persistent(ws: &Workspace, deps: &BTreeMap<TaskId, BTreeSet<TaskId>>) -> Result<()> {
    deps.iter()
        .flat_map(|(id, ds)| ds.iter().map(move |d| (id, d)))
        .find(|(_, d)| ws.target(d).persistent)
        .map_or(Ok(()), |(task, dependency)| {
            Err(Error::PersistentDependency {
                task: task.to_string(),
                dependency: dependency.to_string(),
            })
        })
}

fn invert(deps: &BTreeMap<TaskId, BTreeSet<TaskId>>) -> BTreeMap<TaskId, BTreeSet<TaskId>> {
    deps.iter()
        .flat_map(|(id, ds)| ds.iter().map(move |d| (d.clone(), id.clone())))
        .fold(BTreeMap::new(), |mut map, (dep, id)| {
            map.entry(dep).or_insert_with(BTreeSet::new).insert(id);
            map
        })
}

fn topo_order(deps: &BTreeMap<TaskId, BTreeSet<TaskId>>) -> Result<Vec<TaskId>> {
    let dependents = invert(deps);
    let mut waiting: BTreeMap<&TaskId, usize> = deps.iter().map(|(id, d)| (id, d.len())).collect();
    let mut ready: VecDeque<&TaskId> = waiting
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut order = Vec::with_capacity(deps.len());
    while let Some(id) = ready.pop_front() {
        order.push(id.clone());
        for dependent in dependents.get(id).into_iter().flatten() {
            let n = waiting.get_mut(dependent).expect("dependents are tasks");
            *n -= 1;
            if *n == 0 {
                ready.push_back(dependent);
            }
        }
    }
    if order.len() == deps.len() {
        Ok(order)
    } else {
        Err(Error::Cycle(find_cycle(deps, &order)))
    }
}

/// Every unordered task has an unordered dependency, so following them must loop.
fn find_cycle(deps: &BTreeMap<TaskId, BTreeSet<TaskId>>, ordered: &[TaskId]) -> String {
    let done: BTreeSet<&TaskId> = ordered.iter().collect();
    let start = deps
        .keys()
        .find(|id| !done.contains(id))
        .expect("a cycle leaves tasks unordered");
    let mut path = vec![start];
    loop {
        let last = *path.last().expect("path starts non-empty");
        let next = deps[last]
            .iter()
            .find(|d| !done.contains(d))
            .expect("unordered tasks have unordered dependencies");
        if let Some(i) = path.iter().position(|p| *p == next) {
            return path[i..]
                .iter()
                .copied()
                .chain(std::iter::once(next))
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" -> ");
        }
        path.push(next);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::graph::testing::{project, workspace};

    fn order(g: &TaskGraph) -> Vec<String> {
        g.order.iter().map(ToString::to_string).collect()
    }

    fn chain() -> Workspace {
        workspace(vec![
            project(
                "app",
                &["lib"],
                &[("build", &["^build"]), ("test", &["build"])],
            ),
            project("lib", &["util"], &[("build", &["^build"])]),
            project("util", &[], &[("build", &[])]),
            project("tool", &[], &[("build", &[])]),
        ])
    }

    #[test]
    fn caret_pulls_the_target_from_dependencies_first() {
        let g = TaskGraph::build(&chain(), &["test".into()], None).unwrap();
        assert_eq!(
            order(&g),
            ["util:build", "lib:build", "app:build", "app:test"]
        );
    }

    #[test]
    fn caret_looks_through_dependencies_without_the_target() {
        let ws = workspace(vec![
            project("app", &["types"], &[("build", &["^build"])]),
            project("types", &["util"], &[]),
            project("util", &[], &[("build", &[])]),
        ]);
        let g = TaskGraph::build(
            &ws,
            &["build".into()],
            Some(&BTreeSet::from(["app".into()])),
        )
        .unwrap();
        assert_eq!(order(&g), ["util:build", "app:build"]);
    }

    #[test]
    fn missing_same_project_target_is_skipped() {
        let ws = workspace(vec![project("app", &[], &[("test", &["build"])])]);
        let g = TaskGraph::build(&ws, &["test".into()], None).unwrap();
        assert_eq!(order(&g), ["app:test"]);
    }

    #[test]
    fn project_filter_limits_roots_but_keeps_dependencies() {
        let g = TaskGraph::build(
            &chain(),
            &["build".into()],
            Some(&BTreeSet::from(["app".into()])),
        )
        .unwrap();
        assert_eq!(order(&g), ["util:build", "lib:build", "app:build"]);
    }

    #[test]
    fn unknown_targets_and_projects_are_errors() {
        assert!(matches!(
            TaskGraph::build(&chain(), &["deploy".into()], None),
            Err(Error::UnknownTarget(t)) if t == "deploy"
        ));
        assert!(matches!(
            TaskGraph::build(&chain(), &["build".into()], Some(&BTreeSet::from(["ghost".into()]))),
            Err(Error::UnknownProject(p)) if p == "ghost"
        ));
    }

    #[test]
    fn a_target_no_selected_project_has_is_unknown() {
        let ws = workspace(vec![
            project("app", &[], &[("build", &[])]),
            project("web", &[], &[("deploy", &[])]),
        ]);
        assert!(matches!(
            TaskGraph::build(&ws, &["deploy".into()], Some(&BTreeSet::from(["app".into()]))),
            Err(Error::UnknownTarget(t)) if t == "deploy"
        ));
    }

    #[test]
    fn cycles_are_reported_with_their_path() {
        let ws = workspace(vec![
            project("a", &["b"], &[("build", &["^build"])]),
            project("b", &["a"], &[("build", &["^build"])]),
        ]);
        let Err(Error::Cycle(path)) = TaskGraph::build(&ws, &["build".into()], None) else {
            panic!("expected a cycle");
        };
        assert_eq!(path, "a:build -> b:build -> a:build");
    }

    #[test]
    fn a_self_dependency_is_a_cycle() {
        let ws = workspace(vec![project("a", &[], &[("build", &["build"])])]);
        assert!(matches!(
            TaskGraph::build(&ws, &["build".into()], None),
            Err(Error::Cycle(path)) if path == "a:build -> a:build"
        ));
    }

    #[test]
    fn dev_deps_never_order_tasks() {
        let mut core = project(
            "core",
            &[],
            &[("build", &["^build"]), ("test", &["build", "^build"])],
        );
        core.dev_deps = BTreeSet::from(["test-utils".to_string()]);
        let ws = workspace(vec![
            core,
            project("test-utils", &["core"], &[("build", &["^build"])]),
        ]);
        let g = TaskGraph::build(&ws, &["build".into(), "test".into()], None).unwrap();
        assert_eq!(
            g.deps[&TaskId::new("core", "test")],
            BTreeSet::from([TaskId::new("core", "build")])
        );
        assert_eq!(order(&g), ["core:build", "core:test", "test-utils:build"]);
    }

    #[test]
    fn a_diamond_orders_the_shared_dependency_first() {
        let ws = workspace(vec![
            project("app", &["l1", "l2"], &[("build", &["^build"])]),
            project("l1", &["util"], &[("build", &["^build"])]),
            project("l2", &["util"], &[("build", &["^build"])]),
            project("util", &[], &[("build", &[])]),
        ]);
        let g = TaskGraph::build(&ws, &["build".into()], None).unwrap();
        assert_eq!(order(&g)[0], "util:build");
        assert_eq!(g.deps[&TaskId::new("app", "build")].len(), 2);
    }

    #[test]
    fn a_diamond_through_projects_without_the_target_gives_one_edge() {
        let ws = workspace(vec![
            project("app", &["l1", "l2"], &[("build", &["^build"])]),
            project("l1", &["util"], &[]),
            project("l2", &["util"], &[]),
            project("util", &[], &[("build", &[])]),
        ]);
        let g = TaskGraph::build(&ws, &["build".into()], None).unwrap();
        assert_eq!(
            g.deps[&TaskId::new("app", "build")],
            BTreeSet::from([TaskId::new("util", "build")])
        );
    }

    #[test]
    fn root_order_does_not_change_the_task_order() {
        let roots = vec![
            TaskId::new("app", "test"),
            TaskId::new("lib", "build"),
            TaskId::new("tool", "build"),
            TaskId::new("util", "build"),
        ];
        let reversed = roots.iter().rev().cloned().collect();
        assert_eq!(
            TaskGraph::from_roots(&chain(), roots).unwrap().order,
            TaskGraph::from_roots(&chain(), reversed).unwrap().order
        );
    }

    fn persistent(mut p: Project, target: &str) -> Project {
        p.targets
            .get_mut(target)
            .expect("the target exists")
            .persistent = true;
        p
    }

    #[test]
    fn a_same_project_persistent_dependency_is_an_error() {
        let ws = workspace(vec![persistent(
            project("app", &[], &[("serve", &[]), ("e2e", &["serve"])]),
            "serve",
        )]);
        assert!(matches!(
            TaskGraph::build(&ws, &["e2e".into()], None),
            Err(Error::PersistentDependency { task, dependency })
                if task == "app:e2e" && dependency == "app:serve"
        ));
    }

    #[test]
    fn a_caret_persistent_dependency_is_an_error() {
        let ws = workspace(vec![
            project("app", &["api"], &[("dev", &["^serve"])]),
            persistent(project("api", &[], &[("serve", &[])]), "serve"),
        ]);
        assert!(matches!(
            TaskGraph::build(&ws, &["dev".into()], None),
            Err(Error::PersistentDependency { task, dependency })
                if task == "app:dev" && dependency == "api:serve"
        ));
    }

    #[test]
    fn persistent_roots_are_allowed() {
        let ws = workspace(vec![persistent(
            project("app", &[], &[("serve", &[])]),
            "serve",
        )]);
        let g = TaskGraph::build(&ws, &["serve".into()], None).unwrap();
        assert_eq!(order(&g), ["app:serve"]);
    }

    #[test]
    fn from_roots_rejects_unknown_tasks() {
        assert!(matches!(
            TaskGraph::from_roots(&chain(), vec![TaskId::new("ghost", "build")]),
            Err(Error::UnknownTask(t)) if t == "ghost:build"
        ));
        assert!(matches!(
            TaskGraph::from_roots(&chain(), vec![TaskId::new("app", "deploy")]),
            Err(Error::UnknownTask(t)) if t == "app:deploy"
        ));
    }

    thread_local! {
        static UPSTREAM_LOOKUPS: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn record_upstream_lookup(project: &str, target: &str) {
        UPSTREAM_LOOKUPS.with(|l| l.borrow_mut().push((project.into(), target.into())));
    }

    #[test]
    fn each_caret_lookup_is_resolved_once_per_graph() {
        let ws = workspace(vec![
            project(
                "app",
                &["lib"],
                &[
                    ("build", &["^build"]),
                    ("lint", &["^build"]),
                    ("test", &["^build"]),
                ],
            ),
            project("lib", &[], &[("build", &[])]),
        ]);
        TaskGraph::build(&ws, &["build".into(), "lint".into(), "test".into()], None).unwrap();
        assert_eq!(
            UPSTREAM_LOOKUPS.take(),
            [("app".to_string(), "build".to_string())]
        );
    }

    #[test]
    fn dependents_invert_deps() {
        let g = TaskGraph::build(&chain(), &["test".into()], None).unwrap();
        let dependents = g.dependents();
        assert_eq!(
            dependents[&TaskId::new("app", "build")],
            BTreeSet::from([TaskId::new("app", "test")])
        );
        assert!(!dependents.contains_key(&TaskId::new("app", "test")));
    }

    #[test]
    fn task_ids_serialize_as_strings() {
        let map = BTreeMap::from([(TaskId::new("a", "b"), 1)]);
        assert_eq!(serde_json::to_string(&map).unwrap(), r#"{"a:b":1}"#);
    }
}
