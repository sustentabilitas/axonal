//! TypeScript/JavaScript import edges: oxc-parsed imports resolved through tsconfig
//! `paths`, relative paths and workspace package names.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Bound,
    path::{Path, PathBuf},
};

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::Value;

use crate::{
    error::{Error, Result},
    files::{self, Owners},
};

const EXTENSIONS: [&str; 8] = ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];
const ALIAS_SUFFIXES: [&str; 9] = [
    ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".d.ts",
];

const TEST_INFIXES: [&str; 3] = [".test.", ".spec.", ".test-d."];
const TEST_DIRS: [&str; 7] = [
    "__tests__",
    "__mocks__",
    "__fixtures__",
    "test",
    "tests",
    "spec",
    "e2e",
];

const ROOT_CONFIGS: [&str; 2] = ["tsconfig.base.json", "tsconfig.json"];
const MAX_EXTENDS: usize = 16;

/// Strips `//` and `/* */` comments and trailing commas so tsconfig files parse as JSON.
pub fn strip_jsonc(src: &str) -> String {
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                out.extend(chars.next());
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        let next = chars.peek().copied();
        match (c, next) {
            ('/', Some('/')) => while chars.next_if(|&n| n != '\n').is_some() {},
            ('/', Some('*')) => {
                chars.next();
                let mut prev = '\0';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            _ => {
                in_string = c == '"';
                out.push(c);
            }
        }
    }
    remove_trailing_commas(&out)
}

fn remove_trailing_commas(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut in_string = false;
    let mut escaped = false;
    for (i, &c) in chars.iter().enumerate() {
        if in_string {
            in_string = escaped || c != '"';
            escaped = !escaped && c == '\\';
        } else if c == '"' {
            in_string = true;
        } else if c == ','
            && chars[i + 1..]
                .iter()
                .find(|n| !n.is_whitespace())
                .is_some_and(|n| matches!(n, '}' | ']'))
        {
            continue;
        }
        out.push(c);
    }
    out
}

/// `compilerOptions.paths` from the root tsconfig and its relative `extends` chain.
#[derive(Debug, Clone, Default)]
pub struct TsPaths {
    /// Workspace-relative directory the alias targets are relative to.
    base: PathBuf,
    aliases: Vec<(String, Vec<String>)>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct TsConfig {
    extends: Option<Value>,
    #[serde(default)]
    compiler_options: CompilerOptions,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CompilerOptions {
    base_url: Option<String>,
    paths: Option<BTreeMap<String, Vec<String>>>,
}

impl TsPaths {
    /// Reads `tsconfig.base.json` (else `tsconfig.json`) at `root` and its relative `extends`.
    /// As in TypeScript, a file beats the files it extends and later `extends` entries beat
    /// earlier ones; the highest-ranked file that sets `paths` (or `baseUrl`) wins.
    pub fn load(root: &Path) -> Result<TsPaths> {
        let start = ROOT_CONFIGS
            .into_iter()
            .map(PathBuf::from)
            .find(|f| root.join(f).is_file());
        let chain = walk_extends(root, start.into_iter().collect())
            .into_iter()
            .map(|(rel, config)| config.map(|c| (parent_dir(&rel), c)))
            .collect::<Result<Vec<_>>>()?;
        let paths = chain
            .iter()
            .find_map(|(dir, c)| c.compiler_options.paths.as_ref().map(|p| (dir, p)));
        let base_url = chain.iter().find_map(|(dir, c)| {
            c.compiler_options
                .base_url
                .as_ref()
                .and_then(|b| files::normalize(&dir.join(b)))
        });
        Ok(paths.map_or_else(TsPaths::default, |(dir, paths)| TsPaths {
            base: base_url.unwrap_or_else(|| dir.clone()),
            aliases: paths.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        }))
    }

    /// Workspace-relative candidate paths for a bare specifier, from the single pattern
    /// TypeScript would pick: an exact alias, else the wildcard with the longest prefix.
    pub fn resolve(&self, spec: &str) -> Vec<PathBuf> {
        let exact = self
            .aliases
            .iter()
            .find(|(pattern, _)| pattern == spec)
            .map(|(_, targets)| (targets, ""));
        let chosen = exact.or_else(|| {
            self.aliases
                .iter()
                .filter_map(|(pattern, targets)| {
                    let (prefix, suffix) = pattern.split_once('*')?;
                    let star = spec.strip_prefix(prefix)?.strip_suffix(suffix)?;
                    Some((prefix.len(), targets, star))
                })
                .max_by_key(|(prefix_len, ..)| *prefix_len)
                .map(|(_, targets, star)| (targets, star))
        });
        chosen
            .into_iter()
            .flat_map(|(targets, star)| {
                targets.iter().filter_map(move |t| {
                    files::normalize(&self.base.join(t.replacen('*', star, 1)))
                })
            })
            .collect()
    }
}

/// Every root tsconfig (`tsconfig.base.json`, `tsconfig.json`) and the files their relative
/// `extends` chains name, whether or not they exist or parse. Workspace-relative, sorted.
pub fn config_files(root: &Path) -> Vec<PathBuf> {
    let starts = ROOT_CONFIGS
        .into_iter()
        .map(PathBuf::from)
        .filter(|f| root.join(f).is_file())
        .collect();
    walk_extends(root, starts)
        .into_iter()
        .map(|(rel, _)| rel)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The configs reachable from `starts` through relative `extends`, highest rank first,
/// each with its parse result; an unreadable or invalid config ends its branch.
fn walk_extends(root: &Path, starts: Vec<PathBuf>) -> Vec<(PathBuf, Result<TsConfig>)> {
    let mut chain: Vec<(PathBuf, Result<TsConfig>)> = Vec::new();
    let mut visited = BTreeSet::new();
    let mut pending = starts;
    // Depth-first with the last `extends` entry popped first yields highest rank first.
    while let Some(rel) = pending.pop().filter(|_| chain.len() < MAX_EXTENDS) {
        if !visited.insert(rel.clone()) {
            continue;
        }
        let config = read_config(&root.join(&rel));
        if let Ok(config) = &config {
            let dir = parent_dir(&rel);
            pending.extend(
                config
                    .extends
                    .iter()
                    .flat_map(|extends| relative_extends(extends, &dir)),
            );
        }
        chain.push((rel, config));
    }
    chain
}

fn read_config(path: &Path) -> Result<TsConfig> {
    let config_error = |message: String| Error::Config {
        path: path.to_path_buf(),
        message,
    };
    let src = std::fs::read_to_string(path).map_err(|e| config_error(e.to_string()))?;
    serde_json::from_str(&strip_jsonc(&src)).map_err(|e| config_error(e.to_string()))
}

fn parent_dir(rel: &Path) -> PathBuf {
    rel.parent().map(Path::to_path_buf).unwrap_or_default()
}

/// Workspace-relative files named by the relative entries of an `extends` string or array.
fn relative_extends<'a>(value: &'a Value, dir: &'a Path) -> impl Iterator<Item = PathBuf> + 'a {
    let entries = match value {
        Value::Array(items) => items.as_slice(),
        other => std::slice::from_ref(other),
    };
    entries
        .iter()
        .filter_map(Value::as_str)
        .filter(|e| e.starts_with('.'))
        .filter_map(move |e| {
            let file = if e.ends_with(".json") {
                e.to_string()
            } else {
                format!("{e}.json")
            };
            files::normalize(&dir.join(file))
        })
}

pub fn is_source(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e))
}

/// Module specifiers a source imports or re-exports, plus literal dynamic `import()`s.
pub fn specifiers(path: &Path, src: &str) -> Vec<String> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::tsx());
    let source_type = if source_type.is_javascript() {
        source_type.with_jsx(true)
    } else {
        source_type
    };
    let parsed = Parser::new(&allocator, src, source_type).parse();
    let record = &parsed.module_record;
    let dynamic = record.dynamic_imports.iter().filter_map(|d| {
        let text = &src[d.module_request.start as usize..d.module_request.end as usize];
        let bytes = text.as_bytes();
        let quoted = bytes.len() >= 2
            && matches!(bytes[0], b'"' | b'\'' | b'`')
            && bytes[0] == bytes[bytes.len() - 1];
        let inner = quoted.then(|| &text[1..text.len() - 1])?;
        (!inner.contains(char::from(bytes[0])) && !inner.contains("${")).then(|| inner.to_string())
    });
    record
        .requested_modules
        .keys()
        .map(|k| k.to_string())
        .chain(dynamic)
        .collect()
}

/// `@scope/name/sub` → `@scope/name`; `name/sub` → `name`.
fn package_name(spec: &str) -> &str {
    let segments = if spec.starts_with('@') { 2 } else { 1 };
    spec.match_indices('/')
        .nth(segments - 1)
        .map_or(spec, |(i, _)| &spec[..i])
}

/// Whether an alias target names a workspace file: the file itself, the file with a
/// source extension, or a directory containing files (an index or package entry).
fn exists(target: &Path, workspace: &BTreeSet<&Path>) -> bool {
    let under = workspace
        .range::<Path, _>((Bound::Included(target), Bound::Unbounded))
        .next()
        .is_some_and(|f| f.starts_with(target));
    under
        || ALIAS_SUFFIXES.iter().any(|suffix| {
            let mut file = target.as_os_str().to_owned();
            file.push(suffix);
            workspace.contains(Path::new(&file))
        })
}

/// How an import reached a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Via {
    /// A relative path or tsconfig alias: the project's sources.
    Source,
    /// A package name: possibly the built package (`main`), which needs its build first.
    Package,
}

fn resolve(
    spec: &str,
    file: &Path,
    owners: &Owners,
    packages: &BTreeMap<String, PathBuf>,
    paths: &TsPaths,
    workspace: &BTreeSet<&Path>,
) -> Vec<(PathBuf, Via)> {
    let owner = |p: &Path| owners.owner(p).map(|o| (o.to_path_buf(), Via::Source));
    if spec.starts_with('.') {
        return file
            .parent()
            .and_then(|dir| files::normalize(&dir.join(spec)))
            .and_then(|p| owner(&p))
            .into_iter()
            .collect();
    }
    paths
        .resolve(spec)
        .iter()
        .filter(|p| exists(p, workspace))
        .filter_map(|p| owner(p.as_path()))
        .chain(
            packages
                .get(package_name(spec))
                .map(|r| (r.clone(), Via::Package)),
        )
        .collect()
}

/// Whether a file is test code. `path` is relative to its project root, so a project
/// living at `libs/test` isn't all tests.
pub fn is_test_file(path: &Path) -> bool {
    let named = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| TEST_INFIXES.iter().any(|infix| n.contains(infix)));
    named
        || path.parent().is_some_and(|dir| {
            dir.components()
                .filter_map(|c| c.as_os_str().to_str())
                .any(|c| TEST_DIRS.contains(&c))
        })
}

/// The project roots one project imports from. A root is a dev dep when test files are
/// its only importers and all of them reach it by relative path or tsconfig alias; any
/// package-name import may need the built package, so it makes the root a dep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edges {
    pub deps: BTreeSet<PathBuf>,
    pub dev_deps: BTreeSet<PathBuf>,
}

impl Edges {
    fn add(&mut self, dep: PathBuf, dev: bool) {
        if !dev {
            self.dev_deps.remove(&dep);
            self.deps.insert(dep);
        } else if !self.deps.contains(&dep) {
            self.dev_deps.insert(dep);
        }
    }
}

/// For each project root, the other project roots its TS/JS sources import from.
/// `owned` maps project roots to their files; `packages` maps package names to roots.
pub fn import_edges(
    root: &Path,
    owned: &BTreeMap<PathBuf, Vec<PathBuf>>,
    owners: &Owners,
    packages: &BTreeMap<String, PathBuf>,
    paths: &TsPaths,
) -> BTreeMap<PathBuf, Edges> {
    let workspace: BTreeSet<&Path> = owned.values().flatten().map(PathBuf::as_path).collect();
    let sources: Vec<(&PathBuf, &PathBuf)> = owned
        .iter()
        .flat_map(|(project, files)| {
            files
                .iter()
                .filter(|f| is_source(f))
                .map(move |f| (project, f))
        })
        .collect();
    sources
        .into_par_iter()
        .flat_map_iter(|(project, file)| {
            let from_test = file.strip_prefix(project).is_ok_and(is_test_file);
            let bytes = std::fs::read(root.join(file)).unwrap_or_default();
            specifiers(file, &String::from_utf8_lossy(&bytes))
                .iter()
                .flat_map(|spec| resolve(spec, file, owners, packages, paths, &workspace))
                .filter(|(dep, _)| dep != project)
                .map(|(dep, via)| (project.clone(), dep, from_test && via == Via::Source))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
        .into_iter()
        .fold(BTreeMap::new(), |mut edges, (project, dep, dev)| {
            edges
                .entry(project)
                .or_insert_with(Edges::default)
                .add(dep, dev);
            edges
        })
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
    fn strips_comments_and_trailing_commas_outside_strings() {
        let src = r#"{
  // line comment
  "a": "http://x/*y*/", /* block */
  "b": [1, 2,],
  "c": "quote \" // not a comment",
}"#;
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(src)).unwrap();
        assert_eq!(v["a"], "http://x/*y*/");
        assert_eq!(v["b"], serde_json::json!([1, 2]));
        assert_eq!(v["c"], "quote \" // not a comment");
    }

    #[test]
    fn loads_paths_through_extends() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "tsconfig.base.json",
            r#"{ "extends": "./tsconfig.paths", "compilerOptions": { "strict": true } }"#,
        );
        write(
            dir.path(),
            "tsconfig.paths.json",
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": {
                "@acme/money": ["libs/money/src/index.ts"],
                "@acme/feat/*": ["libs/feat/*/src/index.ts"] } } }"#,
        );
        let paths = TsPaths::load(dir.path()).unwrap();
        assert_eq!(
            paths.resolve("@acme/money"),
            vec![PathBuf::from("libs/money/src/index.ts")]
        );
        assert_eq!(
            paths.resolve("@acme/feat/checkout"),
            vec![PathBuf::from("libs/feat/checkout/src/index.ts")]
        );
        assert!(paths.resolve("react").is_empty());
    }

    #[test]
    fn paths_are_relative_to_the_defining_file_without_base_url() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "tsconfig.json",
            r#"{ "extends": ["./config/base.json"] }"#,
        );
        write(
            dir.path(),
            "config/base.json",
            r#"{ "compilerOptions": { "paths": { "@x": ["../libs/x/index.ts"] } } }"#,
        );
        let paths = TsPaths::load(dir.path()).unwrap();
        assert_eq!(paths.resolve("@x"), vec![PathBuf::from("libs/x/index.ts")]);
    }

    #[test]
    fn config_files_follow_every_root_config_leniently() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "tsconfig.json",
            r#"{ "extends": ["./configs/base", "@tsconfig/node20"] }"#,
        );
        write(
            dir.path(),
            "configs/base.json",
            r#"{ "extends": "./strict.json", }"#,
        );
        write(dir.path(), "configs/strict.json", "{ not json");
        write(
            dir.path(),
            "tsconfig.base.json",
            r#"{ "extends": "./missing" }"#,
        );
        assert_eq!(
            config_files(dir.path()),
            [
                "configs/base.json",
                "configs/strict.json",
                "missing.json",
                "tsconfig.base.json",
                "tsconfig.json"
            ]
            .map(PathBuf::from)
        );
        assert!(config_files(tempfile::tempdir().unwrap().path()).is_empty());
    }

    #[test]
    fn no_tsconfig_means_no_aliases() {
        let dir = tempfile::tempdir().unwrap();
        assert!(TsPaths::load(dir.path()).unwrap().resolve("@x").is_empty());
    }

    #[test]
    fn invalid_tsconfig_is_a_config_error() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "tsconfig.json", "{ nope");
        assert!(matches!(
            TsPaths::load(dir.path()),
            Err(Error::Config { .. })
        ));
    }

    #[test]
    fn strips_a_leading_byte_order_mark() {
        let v: serde_json::Value =
            serde_json::from_str(&strip_jsonc("\u{feff}{ \"a\": 1 }")).unwrap();
        assert_eq!(v["a"], 1);
    }

    fn aliases(entries: &[(&str, &[&str])]) -> TsPaths {
        TsPaths {
            base: PathBuf::new(),
            aliases: entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.iter().map(|t| t.to_string()).collect()))
                .collect(),
        }
    }

    #[test]
    fn resolve_uses_the_longest_wildcard_prefix_only() {
        let paths = aliases(&[
            ("*", &["types/*"]),
            ("@acme/*", &["libs/*"]),
            ("@acme/feat/*", &["feat/*/src"]),
        ]);
        assert_eq!(
            paths.resolve("@acme/feat/a"),
            vec![PathBuf::from("feat/a/src")]
        );
        assert_eq!(paths.resolve("@acme/x"), vec![PathBuf::from("libs/x")]);
        assert_eq!(paths.resolve("react"), vec![PathBuf::from("types/react")]);
    }

    #[test]
    fn exact_alias_beats_wildcards() {
        let paths = aliases(&[
            ("@acme/*", &["libs/*"]),
            ("@acme/money", &["money/index.ts"]),
        ]);
        assert_eq!(
            paths.resolve("@acme/money"),
            vec![PathBuf::from("money/index.ts")]
        );
    }

    #[test]
    fn array_extends_skips_package_entries() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "tsconfig.json",
            r#"{ "extends": ["./paths.json", "@tsconfig/node20/tsconfig.json"] }"#,
        );
        write(
            dir.path(),
            "paths.json",
            r#"{ "compilerOptions": { "paths": { "@x": ["x/index.ts"] } } }"#,
        );
        let paths = TsPaths::load(dir.path()).unwrap();
        assert_eq!(paths.resolve("@x"), vec![PathBuf::from("x/index.ts")]);
    }

    #[test]
    fn later_array_extends_entries_take_precedence() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "tsconfig.json",
            r#"{ "extends": ["./a.json", "./b.json"] }"#,
        );
        write(
            dir.path(),
            "a.json",
            r#"{ "compilerOptions": { "baseUrl": "a", "paths": { "@a": ["a.ts"] } } }"#,
        );
        write(
            dir.path(),
            "b.json",
            r#"{ "compilerOptions": { "paths": { "@b": ["b.ts"] } } }"#,
        );
        let paths = TsPaths::load(dir.path()).unwrap();
        assert!(paths.resolve("@a").is_empty());
        assert_eq!(paths.resolve("@b"), vec![PathBuf::from("a/b.ts")]);
    }

    #[test]
    fn extends_cycles_terminate() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "tsconfig.json",
            r#"{ "extends": "./a", "compilerOptions": { "paths": { "@x": ["x.ts"] } } }"#,
        );
        write(dir.path(), "a.json", r#"{ "extends": "./tsconfig.json" }"#);
        let paths = TsPaths::load(dir.path()).unwrap();
        assert_eq!(paths.resolve("@x"), vec![PathBuf::from("x.ts")]);
    }

    #[test]
    fn missing_extends_is_a_config_error_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "tsconfig.json", r#"{ "extends": "./missing" }"#);
        match TsPaths::load(dir.path()) {
            Err(Error::Config { path, .. }) => assert!(path.ends_with("missing.json")),
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    #[test]
    fn extracts_static_reexport_and_literal_dynamic_imports() {
        let src = r#"import a from "@acme/money";
import type { T } from "./types";
export * from "../shared";
export { b } from "@acme/b";
const lazy = () => import("./lazy");
const dynamic = (x: string) => import(x);
const tpl = () => import(`./tpl`);
const joined = () => import('../x/' + 'y');
const glued = () => import("a" + "b");
"#;
        let mut specs = specifiers(Path::new("src/index.ts"), src);
        specs.sort();
        assert_eq!(
            specs,
            [
                "../shared",
                "./lazy",
                "./tpl",
                "./types",
                "@acme/b",
                "@acme/money"
            ]
        );
    }

    #[test]
    fn parses_jsx_in_tsx_files() {
        let src = "import { B } from '@acme/ui';\nexport const A = () => <B />;\n";
        assert_eq!(specifiers(Path::new("a.tsx"), src), ["@acme/ui"]);
    }

    #[test]
    fn parses_jsx_in_js_files() {
        let src = "import '@acme/ui';\nconst A = () => <B />;\nimport z from '@acme/late';\n";
        let mut specs = specifiers(Path::new("a.js"), src);
        specs.sort();
        assert_eq!(specs, ["@acme/late", "@acme/ui"]);
    }

    #[test]
    fn package_names_keep_their_scope() {
        assert_eq!(package_name("@acme/ui/button"), "@acme/ui");
        assert_eq!(package_name("lodash/fp"), "lodash");
        assert_eq!(package_name("react"), "react");
    }

    fn owned_files(root: &Path, owners: &Owners) -> BTreeMap<PathBuf, Vec<PathBuf>> {
        crate::files::list(root)
            .unwrap()
            .into_iter()
            .filter_map(|f| owners.owner(&f).map(|r| (r.to_path_buf(), f.clone())))
            .fold(BTreeMap::new(), |mut owned, (r, f)| {
                owned.entry(r).or_insert_with(Vec::new).push(f);
                owned
            })
    }

    #[test]
    fn edges_resolve_relative_alias_and_package_imports() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "tsconfig.base.json",
            r#"{ "compilerOptions": { "paths": { "@acme/money": ["libs/money/src/index.ts"] } } }"#,
        );
        write(root, "libs/money/src/index.ts", "export const cents = 1;\n");
        write(
            root,
            "libs/billing/src/index.ts",
            "import { cents } from '@acme/money';\nimport { x } from './local';\n",
        );
        write(root, "libs/billing/src/local.ts", "export const x = 1;\n");
        write(
            root,
            "apps/shop/src/main.ts",
            "import '../../../libs/billing/src/index';\nimport ui from '@acme/ui/button';\nimport react from 'react';\n",
        );
        write(root, "packages/ui/button.ts", "");
        write(root, "packages/ui/notes.md", "import x from '@acme/money'");

        let roots = ["libs/money", "libs/billing", "apps/shop", "packages/ui"].map(PathBuf::from);
        let owners = Owners::new(roots.clone());
        let owned = owned_files(root, &owners);
        let packages = BTreeMap::from([("@acme/ui".to_string(), PathBuf::from("packages/ui"))]);
        let paths = TsPaths::load(root).unwrap();

        let edges = import_edges(root, &owned, &owners, &packages, &paths);
        assert_eq!(
            edges[Path::new("libs/billing")].deps,
            BTreeSet::from([PathBuf::from("libs/money")])
        );
        assert_eq!(
            edges[Path::new("apps/shop")].deps,
            BTreeSet::from([PathBuf::from("libs/billing"), PathBuf::from("packages/ui")])
        );
        assert!(!edges.contains_key(Path::new("libs/money")));
        assert!(!edges.contains_key(Path::new("packages/ui")));
    }

    #[test]
    fn alias_targets_count_only_when_a_workspace_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "*": ["types/*"] } } }"#,
        );
        write(root, "types/foo.d.ts", "export {};\n");
        write(root, "types/bar/index.d.ts", "export {};\n");
        write(root, "apps/a/main.ts", "import react from 'react';\n");
        write(root, "apps/b/main.ts", "import foo from 'foo';\n");
        write(root, "apps/c/main.ts", "import bar from 'bar';\n");

        let roots = ["types", "apps/a", "apps/b", "apps/c"].map(PathBuf::from);
        let owners = Owners::new(roots);
        let owned = owned_files(root, &owners);
        let paths = TsPaths::load(root).unwrap();

        let edges = import_edges(root, &owned, &owners, &BTreeMap::new(), &paths);
        assert!(!edges.contains_key(Path::new("apps/a")));
        assert_eq!(
            edges[Path::new("apps/b")].deps,
            BTreeSet::from([PathBuf::from("types")])
        );
        assert_eq!(
            edges[Path::new("apps/c")].deps,
            BTreeSet::from([PathBuf::from("types")])
        );
    }

    #[test]
    fn test_files_are_recognised_by_name_or_directory() {
        for test in [
            "src/a.test.ts",
            "src/a.spec.tsx",
            "src/a.test-d.ts",
            "src/__tests__/a.ts",
            "src/__mocks__/fs.ts",
            "src/__fixtures__/data.ts",
            "test/a.ts",
            "tests/unit/a.ts",
            "spec/a.ts",
            "e2e/login.ts",
        ] {
            assert!(is_test_file(Path::new(test)), "{test}");
        }
        for source in [
            "src/a.ts",
            "src/testing.ts",
            "src/latest/a.ts",
            "src/a.tests.ts",
            "src/specs/a.ts",
            "src/spec.ts",
        ] {
            assert!(!is_test_file(Path::new(source)), "{source}");
        }
    }

    #[test]
    fn imports_only_from_test_files_are_dev_edges() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "libs/a/src/a.test.ts",
            "import '../../b';\nimport '../../c';\n",
        );
        write(root, "libs/a/src/a.ts", "import '../../c';\n");
        write(root, "libs/test/src/t.ts", "import '../../b';\n");
        write(root, "libs/b/index.ts", "");
        write(root, "libs/c/index.ts", "");

        let roots = ["libs/a", "libs/b", "libs/c", "libs/test"].map(PathBuf::from);
        let owners = Owners::new(roots);
        let owned = owned_files(root, &owners);

        let edges = import_edges(root, &owned, &owners, &BTreeMap::new(), &TsPaths::default());
        assert_eq!(
            edges[Path::new("libs/a")],
            Edges {
                deps: BTreeSet::from([PathBuf::from("libs/c")]),
                dev_deps: BTreeSet::from([PathBuf::from("libs/b")]),
            }
        );
        assert_eq!(
            edges[Path::new("libs/test")],
            Edges {
                deps: BTreeSet::from([PathBuf::from("libs/b")]),
                dev_deps: BTreeSet::new(),
            }
        );
    }

    /// The edges of an explicit project `apps/web` whose only file, `x.spec.ts`, imports
    /// `spec`. `@acme/sdk` and `@acme/core` are packages; `@sdk-src` and `@acme/core` are
    /// tsconfig aliases.
    fn spec_edges(spec: &str) -> Edges {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": {
                "@sdk-src": ["libs/sdk/src/index.ts"],
                "@acme/core": ["libs/core/src/index.ts"] } } }"#,
        );
        write(root, "libs/sdk/src/index.ts", "");
        write(root, "libs/core/src/index.ts", "");
        write(root, "apps/web/x.spec.ts", &format!("import '{spec}';\n"));

        let owners = Owners::new(["libs/sdk", "libs/core", "apps/web"].map(PathBuf::from));
        let owned = owned_files(root, &owners);
        let packages = ["sdk", "core"]
            .map(|n| (format!("@acme/{n}"), PathBuf::from(format!("libs/{n}"))))
            .into();
        let paths = TsPaths::load(root).unwrap();
        import_edges(root, &owned, &owners, &packages, &paths)
            .remove(Path::new("apps/web"))
            .unwrap_or_default()
    }

    fn edges(deps: &[&str], dev_deps: &[&str]) -> Edges {
        Edges {
            deps: deps.iter().map(PathBuf::from).collect(),
            dev_deps: dev_deps.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn package_name_imports_from_spec_files_stay_deps() {
        assert_eq!(spec_edges("@acme/sdk"), edges(&["libs/sdk"], &[]));
        assert_eq!(spec_edges("@acme/sdk/client"), edges(&["libs/sdk"], &[]));
        // Also an alias: it may resolve to the built package, so it still orders tasks.
        assert_eq!(spec_edges("@acme/core"), edges(&["libs/core"], &[]));
    }

    #[test]
    fn alias_imports_from_spec_files_are_dev_deps() {
        assert_eq!(spec_edges("@sdk-src"), edges(&[], &["libs/sdk"]));
    }

    #[test]
    fn relative_imports_from_spec_files_are_dev_deps() {
        assert_eq!(
            spec_edges("../../libs/sdk/src/index"),
            edges(&[], &["libs/sdk"])
        );
    }
}
