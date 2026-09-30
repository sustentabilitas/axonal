//! TypeScript/JavaScript import edges: oxc-parsed imports resolved through tsconfig
//! `paths`, relative paths and workspace package names.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_json::Value;

use crate::{
    error::{Error, Result},
    files,
};

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
}
