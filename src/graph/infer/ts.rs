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
        let mut chain: Vec<(PathBuf, TsConfig)> = Vec::new();
        let mut visited = BTreeSet::new();
        let mut pending: Vec<PathBuf> = ROOT_CONFIGS
            .into_iter()
            .map(PathBuf::from)
            .find(|f| root.join(f).is_file())
            .into_iter()
            .collect();
        // Depth-first with the last `extends` entry popped first yields highest rank first.
        while let Some(rel) = pending.pop().filter(|_| chain.len() < MAX_EXTENDS) {
            if !visited.insert(rel.clone()) {
                continue;
            }
            let path = root.join(&rel);
            let config_error = |message: String| Error::Config {
                path: path.clone(),
                message,
            };
            let src = std::fs::read_to_string(&path).map_err(|e| config_error(e.to_string()))?;
            let config: TsConfig = serde_json::from_str(&strip_jsonc(&src))
                .map_err(|e| config_error(e.to_string()))?;
            let dir = rel.parent().map(Path::to_path_buf).unwrap_or_default();
            pending.extend(
                config
                    .extends
                    .iter()
                    .flat_map(|extends| relative_extends(extends, &dir)),
            );
            chain.push((dir, config));
        }
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

fn resolve(
    spec: &str,
    file: &Path,
    owners: &Owners,
    packages: &BTreeMap<String, PathBuf>,
    paths: &TsPaths,
    workspace: &BTreeSet<&Path>,
) -> Vec<PathBuf> {
    let owner = |p: &Path| owners.owner(p).map(Path::to_path_buf);
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
        .chain(packages.get(package_name(spec)).cloned())
        .collect()
}

/// For each project root, the other project roots its TS/JS sources import from.
/// `owned` maps project roots to their files; `packages` maps package names to roots.
pub fn import_edges(
    root: &Path,
    owned: &BTreeMap<PathBuf, Vec<PathBuf>>,
    owners: &Owners,
    packages: &BTreeMap<String, PathBuf>,
    paths: &TsPaths,
) -> BTreeMap<PathBuf, BTreeSet<PathBuf>> {
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
            let bytes = std::fs::read(root.join(file)).unwrap_or_default();
            specifiers(file, &String::from_utf8_lossy(&bytes))
                .iter()
                .flat_map(|spec| resolve(spec, file, owners, packages, paths, &workspace))
                .filter(|dep| dep != project)
                .map(|dep| (project.clone(), dep))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
        .into_iter()
        .fold(BTreeMap::new(), |mut edges, (project, dep)| {
            edges
                .entry(project)
                .or_insert_with(BTreeSet::new)
                .insert(dep);
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
            edges[Path::new("libs/billing")],
            BTreeSet::from([PathBuf::from("libs/money")])
        );
        assert_eq!(
            edges[Path::new("apps/shop")],
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
            edges[Path::new("apps/b")],
            BTreeSet::from([PathBuf::from("types")])
        );
        assert_eq!(
            edges[Path::new("apps/c")],
            BTreeSet::from([PathBuf::from("types")])
        );
    }
}
