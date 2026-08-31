use std::path::PathBuf;

use miette::Diagnostic;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, LitmusError>;

#[derive(Debug, Error, Diagnostic)]
pub enum LitmusError {
    #[error("invalid command arguments: {message}")]
    #[diagnostic(code(litmus::invalid_arguments))]
    InvalidArguments { message: String },

    #[error("invalid cargo-litmus configuration {path}: {message}")]
    #[diagnostic(code(litmus::config))]
    Config { path: PathBuf, message: String },

    #[error("failed to discover a git repository from {path}")]
    #[diagnostic(code(litmus::repo_not_found))]
    RepoNotFound { path: PathBuf },

    #[error("repository has no working directory")]
    #[diagnostic(code(litmus::bare_repo))]
    BareRepo,

    #[error("path is not valid UTF-8: {path}")]
    #[diagnostic(code(litmus::invalid_utf8_path))]
    InvalidUtf8Path { path: PathBuf },

    #[error("failed to access {path}: {source}")]
    #[diagnostic(code(litmus::io))]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read git index: {0}")]
    #[diagnostic(code(litmus::git_index))]
    GitIndex(#[from] git2::Error),

    #[error("failed to parse Rust source {path}: {source}")]
    #[diagnostic(code(litmus::parse))]
    Parse {
        path: PathBuf,
        #[source]
        source: syn::Error,
    },

    #[error("failed to load Cargo metadata for {manifest}: {source}")]
    #[diagnostic(code(litmus::cargo_metadata))]
    CargoMetadata {
        manifest: PathBuf,
        #[source]
        source: cargo_metadata::Error,
    },

    #[error("indexing failed for {count} file(s): {paths}")]
    #[diagnostic(code(litmus::index_failures))]
    IndexFailures { count: usize, paths: String },

    #[error("failed to serialize index metadata: {0}")]
    #[diagnostic(code(litmus::serialize))]
    Serialize(#[source] rkyv::rancor::BoxedError),

    #[error("failed to deserialize index metadata: {0}")]
    #[diagnostic(code(litmus::deserialize))]
    Deserialize(#[source] rkyv::rancor::BoxedError),

    #[error("failed to render JSON output: {0}")]
    #[diagnostic(code(litmus::json))]
    Json(#[from] serde_json::Error),

    #[error("failed to run {command}: {source}")]
    #[diagnostic(code(litmus::command_io))]
    CommandIo {
        command: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{command} failed with exit code {code}: {stderr}")]
    #[diagnostic(code(litmus::command_failed))]
    CommandFailed {
        command: String,
        code: i32,
        stderr: String,
    },

    #[error("failed to parse {source_name}: {message}")]
    #[diagnostic(code(litmus::parse_external_json))]
    ParseExternalJson {
        source_name: String,
        message: String,
    },
}

impl LitmusError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
