//! TypeScript/JavaScript import edges: oxc-parsed imports resolved through tsconfig
//! `paths`, relative paths and workspace package names.

use std::{
    collections::BTreeMap,
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
    /// Reads `tsconfig.base.json` (else `tsconfig.json`) at `root`. The nearest file in the
    /// `extends` chain that sets `paths` (or `baseUrl`) wins, as in TypeScript.
    pub fn load(root: &Path) -> Result<TsPaths> {
        let mut chain: Vec<(PathBuf, TsConfig)> = Vec::new();
        let mut next = ROOT_CONFIGS
            .into_iter()
            .map(PathBuf::from)
            .find(|f| root.join(f).is_file());
        while let Some(rel) = next.take().filter(|_| chain.len() < MAX_EXTENDS) {
            let path = root.join(&rel);
            let config: TsConfig = serde_json::from_str(&strip_jsonc(&std::fs::read_to_string(
                &path,
            )?))
            .map_err(|e| Error::Config {
                path: path.clone(),
                message: e.to_string(),
            })?;
            let dir = rel.parent().map(Path::to_path_buf).unwrap_or_default();
            next = config
                .extends
                .as_ref()
                .and_then(extends_path)
                .filter(|e| e.starts_with('.'))
                .and_then(|e| {
                    let file = if e.ends_with(".json") {
                        e.to_string()
                    } else {
                        format!("{e}.json")
                    };
                    files::normalize(&dir.join(file))
                });
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

    /// Workspace-relative candidate paths for a bare specifier.
    pub fn resolve(&self, spec: &str) -> Vec<PathBuf> {
        self.aliases
            .iter()
            .filter_map(|(pattern, targets)| capture(pattern, spec).map(|star| (targets, star)))
            .flat_map(|(targets, star)| {
                targets.iter().filter_map(move |t| {
                    files::normalize(&self.base.join(t.replacen('*', star, 1)))
                })
            })
            .collect()
    }
}

fn extends_path(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s),
        Value::Array(items) => items.iter().rev().find_map(Value::as_str),
        _ => None,
    }
}

fn capture<'s>(pattern: &str, spec: &'s str) -> Option<&'s str> {
    match pattern.split_once('*') {
        None => (pattern == spec).then_some(""),
        Some((prefix, suffix)) => spec.strip_prefix(prefix)?.strip_suffix(suffix),
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
}
