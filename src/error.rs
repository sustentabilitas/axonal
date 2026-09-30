//! Errors that stop axonal before or instead of running tasks (exit code 2).

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {message}", path.display())]
    Config { path: PathBuf, message: String },
    #[error("duplicate project name `{name}` at `{first}` and `{second}`")]
    DuplicateProject {
        name: String,
        first: String,
        second: String,
    },
    #[error("unknown project `{0}`")]
    UnknownProject(String),
    #[error("unknown target `{0}`")]
    UnknownTarget(String),
    #[error("target `{target}` of project `{project}` has no command")]
    MissingCommand { project: String, target: String },
    #[error("dependency cycle: {0}")]
    Cycle(String),
    #[error("{tool}: {message}")]
    Tool { tool: String, message: String },
    #[error("git: {0}")]
    Git(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
