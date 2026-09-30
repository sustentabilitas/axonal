//! The task graph: `project:target` tasks and the edges `depends_on` implies.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
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
        if let Some(missing) = targets
            .iter()
            .find(|t| !ws.projects.values().any(|p| p.targets.contains_key(*t)))
        {
            return Err(Error::UnknownTarget(missing.clone()));
        }
        if let Some(names) = projects {
            ws.check_projects(names)?;
        }
        let roots = ws
            .projects
            .values()
            .filter(|p| projects.is_none_or(|names| names.contains(&p.name)))
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
        let mut deps = BTreeMap::new();
        let mut stack = roots;
        while let Some(id) = stack.pop() {
            if deps.contains_key(&id) {
                continue;
            }
            let task_deps = task_deps(ws, &id);
            stack.extend(task_deps.iter().cloned());
            deps.insert(id, task_deps);
        }
        let order = topo_order(&deps)?;
        Ok(TaskGraph { deps, order })
    }

    pub fn dependents(&self) -> BTreeMap<TaskId, BTreeSet<TaskId>> {
        invert(&self.deps)
    }
}

fn task_deps(ws: &Workspace, id: &TaskId) -> BTreeSet<TaskId> {
    let project = &ws.projects[&id.project];
    project.targets[&id.target]
        .depends_on
        .iter()
        .flat_map(|dep| match dep.strip_prefix('^') {
            Some(target) => upstream(ws, project, target),
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
    let mut found = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut stack: Vec<&str> = project.deps.iter().map(String::as_str).collect();
    while let Some(name) = stack.pop() {
        if !seen.insert(name) {
            continue;
        }
        let dep = &ws.projects[name];
        if dep.targets.contains_key(target) {
            found.insert(TaskId::new(name, target));
        } else {
            stack.extend(dep.deps.iter().map(String::as_str));
        }
    }
    found
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
    fn cycles_are_reported_with_their_path() {
        let ws = workspace(vec![
            project("a", &["b"], &[("build", &["^build"])]),
            project("b", &["a"], &[("build", &["^build"])]),
        ]);
        let Err(Error::Cycle(path)) = TaskGraph::build(&ws, &["build".into()], None) else {
            panic!("expected a cycle");
        };
        assert!(
            path.contains("a:build") && path.contains("b:build"),
            "{path}"
        );
        assert_eq!(path.matches(" -> ").count(), 2, "{path}");
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
