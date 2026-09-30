//! `axonal.toml`: target defaults, project declarations and overrides, cache settings.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const FILE: &str = "axonal.toml";

const DEFAULT_LOCAL_MAX: u64 = 10 << 30;

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub targets: BTreeMap<String, TargetConfig>,
    #[serde(default)]
    pub projects: BTreeMap<String, ProjectConfig>,
    #[serde(default)]
    pub cache: CacheConfig,
    /// Validated by the pruning sub-project; the core only carries it.
    #[serde(default)]
    pub prune: Option<toml::Table>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    #[serde(default = "default_branch")]
    pub default_branch: String,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            default_branch: default_branch(),
        }
    }
}

fn default_branch() -> String {
    "main".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DepsUsage {
    None,
    Api,
    Impl,
}

impl DepsUsage {
    pub fn default_for(target: &str) -> Self {
        match target {
            "fmt" | "format" => Self::None,
            "lint" | "typecheck" | "check" => Self::Api,
            _ => Self::Impl,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    pub command: Option<String>,
    pub depends_on: Option<Vec<String>>,
    pub inputs: Option<Vec<String>>,
    pub outputs: Option<Vec<String>>,
    pub env: Option<Vec<String>>,
    pub deps_usage: Option<DepsUsage>,
    pub persistent: Option<bool>,
}

impl TargetConfig {
    /// Fields set here win; unset fields come from `fallback`.
    pub fn or(self, fallback: &TargetConfig) -> TargetConfig {
        TargetConfig {
            command: self.command.or_else(|| fallback.command.clone()),
            depends_on: self.depends_on.or_else(|| fallback.depends_on.clone()),
            inputs: self.inputs.or_else(|| fallback.inputs.clone()),
            outputs: self.outputs.or_else(|| fallback.outputs.clone()),
            env: self.env.or_else(|| fallback.env.clone()),
            deps_usage: self.deps_usage.or(fallback.deps_usage),
            persistent: self.persistent.or(fallback.persistent),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Replaces the inferred name (package, crate or path), e.g. to resolve a collision.
    pub name: Option<String>,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default)]
    pub targets: BTreeMap<String, TargetConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// Used by the remote-cache sub-project.
    pub remote: Option<String>,
    pub local_max_size: Option<String>,
}

impl CacheConfig {
    pub fn local_max_bytes(&self) -> Result<u64> {
        self.local_max_size
            .as_deref()
            .map_or(Ok(DEFAULT_LOCAL_MAX), |size| {
                parse_size(size).ok_or_else(|| Error::Config {
                    path: PathBuf::from(FILE),
                    message: format!(
                        "cache.local_max_size: invalid size `{size}` (use e.g. 500MB or 10GB)"
                    ),
                })
            })
    }
}

fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, unit) = text.split_at(split);
    let multiplier: u64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "KB" => 1 << 10,
        "MB" => 1 << 20,
        "GB" => 1 << 30,
        "TB" => 1 << 40,
        _ => return None,
    };
    digits.parse::<u64>().ok()?.checked_mul(multiplier)
}

impl Config {
    /// A missing `axonal.toml` is the empty config: everything is inferred.
    pub fn load(root: &Path) -> Result<Config> {
        let path = root.join(FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text, &path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(Error::Config {
                path,
                message: e.to_string(),
            }),
        }
    }

    pub fn parse(text: &str, path: &Path) -> Result<Config> {
        toml::from_str(text).map_err(|e| Error::Config {
            path: path.to_owned(),
            message: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
[workspace]
default_branch = "trunk"

[targets.build]
depends_on = ["^build"]
inputs = ["src/**", "{workspace}/pnpm-lock.yaml"]
outputs = ["dist/**"]
env = ["NODE_ENV"]

[projects."libs/billing"]
deps = ["libs/money"]

[projects."libs/billing".targets.test]
command = "jest"
deps_usage = "api"

[cache]
local_max_size = "500MB"

[prune]
mode = "shadow"
"#;

    #[test]
    fn parses_every_section() {
        let c = Config::parse(FULL, Path::new(FILE)).unwrap();
        assert_eq!(c.workspace.default_branch, "trunk");
        assert_eq!(
            c.targets["build"].depends_on,
            Some(vec!["^build".to_string()])
        );
        assert_eq!(c.projects["libs/billing"].deps, vec!["libs/money"]);
        assert_eq!(
            c.projects["libs/billing"].targets["test"].deps_usage,
            Some(DepsUsage::Api)
        );
        assert_eq!(c.cache.local_max_bytes().unwrap(), 500 << 20);
        assert!(c.prune.is_some());
    }

    #[test]
    fn empty_and_missing_files_are_the_default() {
        let c = Config::parse("", Path::new(FILE)).unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.workspace.default_branch, "main");
        assert_eq!(c.cache.local_max_bytes().unwrap(), 10 << 30);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(dir.path()).unwrap(), Config::default());
    }

    #[test]
    fn unreadable_files_name_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(FILE)).unwrap();
        let err = Config::load(dir.path()).unwrap_err();
        assert!(matches!(err, Error::Config { .. }), "{err:?}");
        assert!(err.to_string().contains("axonal.toml"), "{err}");
    }

    #[test]
    fn unknown_fields_report_file_and_line() {
        let err = Config::parse(
            "[workspace]\ndefault_branch = \"main\"\nbogus = 1\n",
            Path::new(FILE),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("axonal.toml"), "{msg}");
        assert!(msg.contains("line 3"), "{msg}");
        assert!(msg.contains("bogus"), "{msg}");
    }

    #[test]
    fn projects_can_override_their_name() {
        let c = Config::parse(
            "[projects.\"crates/core\"]\nname = \"core-rs\"\n",
            Path::new(FILE),
        )
        .unwrap();
        assert_eq!(c.projects["crates/core"].name.as_deref(), Some("core-rs"));
        assert!(Config::parse("[projects.x]\nnmae = \"y\"\n", Path::new(FILE)).is_err());
    }

    #[test]
    fn target_or_prefers_its_own_fields() {
        let own = TargetConfig {
            command: Some("a".into()),
            ..TargetConfig::default()
        };
        let fallback = TargetConfig {
            command: Some("b".into()),
            outputs: Some(vec!["dist/**".into()]),
            ..TargetConfig::default()
        };
        let merged = own.or(&fallback);
        assert_eq!(merged.command.as_deref(), Some("a"));
        assert_eq!(merged.outputs, Some(vec!["dist/**".to_string()]));
    }

    #[test]
    fn target_or_keeps_an_explicit_empty_list() {
        let own = TargetConfig {
            depends_on: Some(vec![]),
            ..TargetConfig::default()
        };
        let fallback = TargetConfig {
            depends_on: Some(vec!["^build".into()]),
            ..TargetConfig::default()
        };
        assert_eq!(own.or(&fallback).depends_on, Some(vec![]));
    }

    #[test]
    fn deps_usage_defaults_by_target_name() {
        assert_eq!(DepsUsage::default_for("fmt"), DepsUsage::None);
        assert_eq!(DepsUsage::default_for("lint"), DepsUsage::Api);
        assert_eq!(DepsUsage::default_for("typecheck"), DepsUsage::Api);
        assert_eq!(DepsUsage::default_for("test"), DepsUsage::Impl);
    }

    #[test]
    fn sizes_parse_with_binary_units() {
        assert_eq!(parse_size("10GB"), Some(10 << 30));
        assert_eq!(parse_size("5 mb"), Some(5 << 20));
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("x"), None);
        assert_eq!(parse_size("10PB"), None);
        assert_eq!(parse_size("18446744073709551616"), None);
        assert_eq!(parse_size("99999999TB"), None);
        assert_eq!(parse_size("1.5GB"), None);
    }
}
