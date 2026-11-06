//! Select affected Rust tests from Git changes, widening selection when a
//! narrow result cannot be established safely.
//!
//! This crate powers the `cargo-litmus` CLI and exposes the same pipeline:
//!
//! - [`build_index`] parses tracked Rust sources and Cargo metadata into an
//!   [`IndexMetadata`] graph and writes an `rkyv` cache.
//! - [`affected::compute_affected`] combines that index with changed paths and
//!   line ranges, repository rules, and `cargo-ferris-wheel` dependency
//!   information. Its [`affected::AffectedReport`] contains selections,
//!   reasons, and nextest commands; it does not execute those commands.
//! - [`explain::ExplainReport`] explains path or package selections.
//!
//! Uncertain inputs and failed safety checks widen selection to package,
//! workspace, or repository tests rather than silently skipping them.
//!
//! [`affected::compute_affected_validated`] additionally checks filters with
//! `cargo nextest list`. This may compile code and execute build scripts;
//! use it only with trusted repositories.
//!
//! # Example
//!
//! ```no_run
//! use std::path::PathBuf;
//!
//! use cargo_litmus::affected::{AffectedOptions, compute_affected};
//! use cargo_litmus::changed_files::ChangeSet;
//! use cargo_litmus::ferris::CommandFerrisRunner;
//!
//! let root = PathBuf::from(".");
//! let changes = ChangeSet::worktree(&root)?;
//! let report = compute_affected(
//!     &AffectedOptions {
//!         root,
//!         files: Vec::new(),
//!         changed_files: changes.files,
//!         base_cache: None,
//!         current_cache: None,
//!     },
//!     &CommandFerrisRunner,
//! )?;
//!
//! for cmd in &report.nextest_commands {
//!     println!("{} {:?}", cmd.workspace, cmd.args);
//! }
//! # Ok::<(), cargo_litmus::LitmusError>(())
//! ```

pub mod affected;
pub mod cargo_graph;
pub mod changed_files;
pub mod cli;
pub mod config;
pub mod discovery;
pub mod error;
pub mod explain;
pub mod ferris;
pub mod indexer;
pub mod inert;
pub mod model;
pub mod nextest;
pub mod parser;
pub mod storage;

pub use cli::{Cli, Commands, OutputFormat};
pub use error::{LitmusError, Result};
pub use indexer::{IndexOptions, IndexSummary, build_index};
pub use model::{Edge, EdgeKind, FileNode, IndexMetadata, ItemKind, ItemNode, SpanRange};
