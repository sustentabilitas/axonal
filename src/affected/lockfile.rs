//! Lockfile diffs: which projects' resolved external dependencies changed.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};

use serde::Deserialize;

use crate::graph::{Kind, Workspace};

pub const PNPM_LOCK: &str = "pnpm-lock.yaml";
pub const CARGO_LOCK: &str = "Cargo.lock";

/// Resolved external packages per pnpm importer path or Cargo member name, transitively.
pub type Closures = BTreeMap<String, BTreeSet<String>>;

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PnpmLock {
    lockfile_version: Option<String>,
    #[serde(default)]
    importers: BTreeMap<String, Importer>,
    #[serde(default)]
    snapshots: BTreeMap<String, Snapshot>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Importer {
    #[serde(default)]
    dependencies: BTreeMap<String, Resolved>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, Resolved>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, Resolved>,
}

impl Importer {
    fn merge(&mut self, other: Importer) {
        self.dependencies.extend(other.dependencies);
        self.dev_dependencies.extend(other.dev_dependencies);
        self.optional_dependencies
            .extend(other.optional_dependencies);
    }

    fn all(&self) -> impl Iterator<Item = (&String, &Resolved)> {
        self.dependencies
            .iter()
            .chain(&self.dev_dependencies)
            .chain(&self.optional_dependencies)
    }
}

#[derive(Deserialize)]
struct Resolved {
    version: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
}

/// pnpm 9+ lockfiles, including the multi-document files pnpm 11+ writes.
pub fn pnpm_closures(text: &str) -> Result<Closures, String> {
    let mut importers: BTreeMap<String, Importer> = BTreeMap::new();
    let mut snapshots: BTreeMap<String, Snapshot> = BTreeMap::new();
    for doc in serde_norway::Deserializer::from_str(text) {
        let lock = PnpmLock::deserialize(doc).map_err(|e| e.to_string())?;
        if let Some(version) = &lock.lockfile_version {
            let major = version
                .split('.')
                .next()
                .and_then(|m| m.parse::<u32>().ok());
            if major.is_none_or(|m| m < 9) {
                return Err(format!(
                    "unsupported pnpm lockfile version {version}; axonal reads version 9 or later"
                ));
            }
        }
        for (path, importer) in lock.importers {
            importers.entry(path).or_default().merge(importer);
        }
        snapshots.extend(lock.snapshots);
    }
    Ok(importers
        .into_iter()
        .map(|(path, importer)| {
            let roots = importer
                .all()
                .filter_map(|(name, r)| package_key(name, &r.version))
                .collect();
            (path, closure(roots, &snapshots))
        })
        .collect())
}

/// `name@version`; aliased versions (`real@1.0.0`) are already keys; `link:` is a workspace link.
fn package_key(name: &str, version: &str) -> Option<String> {
    if version.starts_with("link:") {
        return None;
    }
    let base = version.split('(').next().unwrap_or(version);
    Some(if base.contains('@') {
        version.to_string()
    } else {
        format!("{name}@{version}")
    })
}

fn closure(roots: Vec<String>, snapshots: &BTreeMap<String, Snapshot>) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut stack = roots;
    while let Some(key) = stack.pop() {
        if seen.contains(&key) {
            continue;
        }
        if let Some(snapshot) = snapshots.get(&key) {
            stack.extend(
                snapshot
                    .dependencies
                    .iter()
                    .chain(&snapshot.optional_dependencies)
                    .filter_map(|(n, v)| package_key(n, v)),
            );
        }
        seen.insert(key);
    }
    seen
}

#[derive(Deserialize)]
struct CargoLock {
    #[serde(default)]
    package: Vec<LockPackage>,
}

#[derive(Deserialize)]
struct LockPackage {
    name: String,
    version: String,
    source: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

pub fn cargo_closures(text: &str) -> Result<Closures, String> {
    let lock: CargoLock = toml::from_str(text).map_err(|e| e.to_string())?;
    let by_name: BTreeMap<&str, Vec<&LockPackage>> =
        lock.package.iter().fold(BTreeMap::new(), |mut map, p| {
            map.entry(p.name.as_str()).or_default().push(p);
            map
        });
    Ok(lock
        .package
        .iter()
        .filter(|p| p.source.is_none())
        .map(|member| {
            let mut seen: BTreeSet<*const LockPackage> = BTreeSet::new();
            let mut externals = BTreeSet::new();
            let mut stack = vec![member];
            while let Some(package) = stack.pop() {
                for dep in package
                    .dependencies
                    .iter()
                    .filter_map(|d| resolve_cargo_dep(&by_name, d))
                {
                    if seen.insert(std::ptr::from_ref(dep)) {
                        if let Some(source) = &dep.source {
                            externals.insert(format!("{} {} {source}", dep.name, dep.version));
                        }
                        stack.push(dep);
                    }
                }
            }
            (member.name.clone(), externals)
        })
        .collect())
}

/// Cargo.lock dependency strings are `name`, `name version` or `name version (source)`.
fn resolve_cargo_dep<'a>(
    by_name: &BTreeMap<&str, Vec<&'a LockPackage>>,
    dep: &str,
) -> Option<&'a LockPackage> {
    let mut parts = dep.splitn(3, ' ');
    let name = parts.next()?;
    let version = parts.next();
    let source = parts
        .next()
        .and_then(|s| s.strip_prefix('('))
        .and_then(|s| s.strip_suffix(')'));
    by_name.get(name)?.iter().copied().find(|p| {
        version.is_none_or(|v| p.version == v)
            && source.is_none_or(|s| p.source.as_deref() == Some(s))
    })
}

/// Keys whose closure differs; `None` when either side fails to parse.
fn diff(
    old: Option<&str>,
    new: Option<&str>,
    parse: fn(&str) -> Result<Closures, String>,
) -> Option<BTreeSet<String>> {
    let read = |text: Option<&str>| text.map_or_else(|| Ok(Closures::new()), parse).ok();
    let (old, new) = (read(old)?, read(new)?);
    Some(
        old.keys()
            .chain(new.keys())
            .filter(|k| old.get(*k) != new.get(*k))
            .cloned()
            .collect(),
    )
}

/// The pnpm importer key for a project root: `.` for the workspace root, else the
/// `/`-separated relative path.
fn importer(root: &Path) -> String {
    let parts: Vec<_> = root
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy()),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

/// Projects whose external dependencies changed. Each argument is `None` when that lockfile
/// did not change, else its (old, new) contents (`None` where absent). A lockfile that
/// fails to parse, or a change to the root pnpm importer, impacts every project of its
/// ecosystem.
pub fn impacted(
    ws: &Workspace,
    pnpm: Option<(Option<&str>, Option<&str>)>,
    cargo: Option<(Option<&str>, Option<&str>)>,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some((old, new)) = pnpm {
        let js = ws
            .projects
            .values()
            .filter(|p| p.kinds.contains(&Kind::Js) || p.kinds.contains(&Kind::Explicit));
        match diff(old, new, pnpm_closures) {
            Some(changed) if !changed.contains(".") => out.extend(
                js.filter(|p| p.kinds.contains(&Kind::Js) && changed.contains(&importer(&p.root)))
                    .map(|p| p.name.clone()),
            ),
            _ => out.extend(js.map(|p| p.name.clone())),
        }
    }
    if let Some((old, new)) = cargo {
        let crates = ws.projects.values().filter(|p| p.crate_name.is_some());
        match diff(old, new, cargo_closures) {
            Some(changed) => out.extend(
                crates
                    .filter(|p| p.crate_name.as_ref().is_some_and(|c| changed.contains(c)))
                    .map(|p| p.name.clone()),
            ),
            None => out.extend(crates.map(|p| p.name.clone())),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::testing::{project, workspace};

    const PNPM: &str = "lockfileVersion: '9.0'

importers:

  .: {}

  packages/app:
    dependencies:
      '@mixed/ui':
        specifier: workspace:*
        version: link:../ui
      is-odd:
        specifier: 3.0.1
        version: 3.0.1

  packages/ui:
    dependencies:
      left-pad:
        specifier: 1.3.0
        version: 1.3.0

packages:

  is-number@6.0.0:
    resolution: {integrity: sha512-x}

  is-odd@3.0.1:
    resolution: {integrity: sha512-y}

  left-pad@1.3.0:
    resolution: {integrity: sha512-z}

snapshots:

  is-number@6.0.0: {}

  is-odd@3.0.1:
    dependencies:
      is-number: 6.0.0

  left-pad@1.3.0: {}
";

    const CARGO: &str = r#"version = 4

[[package]]
name = "engine"
version = "0.1.0"
dependencies = [
 "itoa",
]

[[package]]
name = "itoa"
version = "1.0.15"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "server"
version = "0.1.0"
dependencies = [
 "engine",
]
"#;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn mixed() -> Workspace {
        let mut app = project("@mixed/app", &["@mixed/ui"], &[]);
        app.root = "packages/app".into();
        app.kinds = BTreeSet::from([Kind::Js]);
        let mut ui = project("@mixed/ui", &[], &[]);
        ui.root = "packages/ui".into();
        ui.kinds = BTreeSet::from([Kind::Js]);
        let mut engine = project("engine", &[], &[]);
        engine.kinds = BTreeSet::from([Kind::Cargo]);
        engine.crate_name = Some("engine".into());
        let mut server = project("server", &["engine"], &[]);
        server.kinds = BTreeSet::from([Kind::Cargo]);
        server.crate_name = Some("server".into());
        workspace(vec![app, ui, engine, server])
    }

    #[test]
    fn pnpm_closures_follow_snapshots_and_skip_links() {
        let c = pnpm_closures(PNPM).unwrap();
        assert_eq!(c["packages/app"], set(&["is-number@6.0.0", "is-odd@3.0.1"]));
        assert_eq!(c["packages/ui"], set(&["left-pad@1.3.0"]));
        assert!(c["."].is_empty());
    }

    #[test]
    fn pnpm_multi_document_lockfiles_merge() {
        let text = format!(
            "---\nlockfileVersion: '9.0'\nimporters:\n  .:\n    configDependencies: {{}}\n---\n{PNPM}"
        );
        assert_eq!(pnpm_closures(&text).unwrap(), pnpm_closures(PNPM).unwrap());
    }

    #[test]
    fn peer_suffixes_and_aliases_resolve() {
        let text = "lockfileVersion: '9.0'
importers:
  web:
    dependencies:
      react-dom:
        specifier: ^19.0.0
        version: 19.0.0(react@19.0.0)
      lodash-alias:
        specifier: npm:lodash-es@4
        version: lodash-es@4.17.21
snapshots:
  react-dom@19.0.0(react@19.0.0):
    dependencies:
      react: 19.0.0
  react@19.0.0: {}
  lodash-es@4.17.21: {}
";
        assert_eq!(
            pnpm_closures(text).unwrap()["web"],
            set(&[
                "lodash-es@4.17.21",
                "react-dom@19.0.0(react@19.0.0)",
                "react@19.0.0"
            ])
        );
    }

    #[test]
    fn old_pnpm_lockfiles_are_rejected() {
        assert!(pnpm_closures("lockfileVersion: '6.0'\n").is_err());
    }

    #[test]
    fn cargo_closures_are_transitive_and_external_only() {
        let c = cargo_closures(CARGO).unwrap();
        let itoa = "itoa 1.0.15 registry+https://github.com/rust-lang/crates.io-index";
        assert_eq!(c["engine"], set(&[itoa]));
        assert_eq!(c["server"], set(&[itoa]));
    }

    #[test]
    fn cargo_dependencies_resolve_by_version_and_source() {
        let text = r#"version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "itoa 1.0.15 (git+https://example.com/itoa#abc)",
]

[[package]]
name = "itoa"
version = "1.0.15"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "itoa"
version = "1.0.15"
source = "git+https://example.com/itoa#abc"
"#;
        assert_eq!(
            cargo_closures(text).unwrap()["app"],
            set(&["itoa 1.0.15 git+https://example.com/itoa#abc"])
        );
    }

    #[test]
    fn impact_is_limited_to_projects_whose_closure_changed() {
        let ws = mixed();
        let bumped = PNPM
            .replace("is-number@6.0.0", "is-number@7.0.0")
            .replace("is-number: 6.0.0", "is-number: 7.0.0");
        assert_eq!(
            impacted(&ws, Some((Some(PNPM), Some(&bumped))), None),
            set(&["@mixed/app"])
        );

        let cargo_bumped = CARGO.replace("1.0.15", "1.0.16");
        assert_eq!(
            impacted(&ws, None, Some((Some(CARGO), Some(&cargo_bumped)))),
            set(&["engine", "server"])
        );
        assert!(impacted(&ws, None, None).is_empty());
    }

    #[test]
    fn added_lockfiles_impact_projects_with_external_dependencies() {
        let ws = mixed();
        assert_eq!(
            impacted(&ws, Some((None, Some(PNPM))), None),
            set(&["@mixed/app", "@mixed/ui"])
        );
        assert_eq!(
            impacted(&ws, None, Some((Some(CARGO), None))),
            set(&["engine", "server"])
        );
    }

    #[test]
    fn root_importer_changes_and_parse_failures_impact_every_js_project() {
        let ws = mixed();
        let root_dep = PNPM.replace(
            "  .: {}\n",
            "  .:\n    devDependencies:\n      left-pad:\n        specifier: 1.3.0\n        version: 1.3.0\n",
        );
        let all_js = set(&["@mixed/app", "@mixed/ui"]);
        assert_eq!(
            impacted(&ws, Some((Some(PNPM), Some(&root_dep))), None),
            all_js
        );
        assert_eq!(
            impacted(&ws, Some((Some(PNPM), Some("lockfileVersion: [\n"))), None),
            all_js
        );
        assert_eq!(
            impacted(&ws, None, Some((Some(CARGO), Some("not toml [")))),
            set(&["engine", "server"])
        );
    }

    #[test]
    fn importer_keys_use_forward_slashes() {
        assert_eq!(importer(Path::new("")), ".");
        assert_eq!(importer(Path::new("packages/app")), "packages/app");
        assert_eq!(importer(Path::new("./packages/app")), "packages/app");
    }
}
