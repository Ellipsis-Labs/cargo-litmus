use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::affected::{
    AffectedOptions, AffectedReport, compute_affected, compute_affected_validated,
};
use crate::changed_files::{
    ChangeSet, ChangedFileInput, merge_changed_file_inputs, parse_changed_file_inputs,
};
use crate::error::{LitmusError, Result};
use crate::explain::ExplainReport;
use crate::ferris::CommandFerrisRunner;
use crate::indexer::{IndexOptions, IndexReport, build_index};

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Select affected Rust tests conservatively from Git changes"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Parse Git-tracked Rust files and write a rkyv index.
    Index {
        /// Repository or subdirectory root to index.
        #[arg(long, default_value = ".")]
        root: PathBuf,

        /// Index cache output path. Defaults to
        /// $CARGO_TARGET_DIR/cargo-litmus.rkyv or target/cargo-litmus.rkyv.
        #[arg(long)]
        cache: Option<PathBuf>,

        /// Output format for stdout.
        #[arg(long, value_enum, default_value_t = OutputFormat::Summary)]
        format: OutputFormat,

        /// Write the full JSON report to this path and print telemetry to
        /// stdout.
        #[arg(long)]
        json_output: Option<PathBuf>,
    },

    /// Report packages and workspaces affected by changed files.
    Affected {
        /// Repository or subdirectory root to analyze.
        #[arg(long, default_value = ".")]
        root: PathBuf,

        /// Previous/base rkyv index to compare against. Defaults to
        /// LITMUS_BASE_CACHE when set.
        #[arg(long)]
        base_cache: Option<PathBuf>,

        /// Current rkyv index output path. Defaults to
        /// $CARGO_TARGET_DIR/cargo-litmus.rkyv or target/cargo-litmus.rkyv.
        #[arg(long)]
        current_cache: Option<PathBuf>,

        /// Changed files to analyze.
        #[arg(long, num_args = 1..)]
        files: Vec<String>,

        /// Base revision for changed-line discovery.
        #[arg(long = "base", visible_alias = "base-rev", requires = "head_rev")]
        base_rev: Option<String>,

        /// Head revision for changed-line discovery.
        #[arg(long = "head", visible_alias = "head-rev", requires = "base_rev")]
        head_rev: Option<String>,

        /// Compare the merge-base of this revision and HEAD with HEAD.
        #[arg(long, conflicts_with_all = ["base_rev", "head_rev", "worktree", "staged"])]
        merge_base: Option<String>,

        /// Analyze staged, unstaged, deleted, renamed, and untracked changes.
        #[arg(long, conflicts_with_all = ["base_rev", "head_rev", "merge_base", "staged"])]
        worktree: bool,

        /// Analyze only changes staged in the Git index.
        #[arg(long, conflicts_with_all = ["base_rev", "head_rev", "merge_base", "worktree"])]
        staged: bool,

        /// Read changed files from stdin as lines, a JSON array, or a JSON
        /// object.
        #[arg(long)]
        files_stdin: bool,

        /// Validate selected filters with cargo-nextest. This may compile
        /// repository code and execute build scripts; use only on trusted code.
        #[arg(long)]
        validate_nextest: bool,

        /// Output format for stdout.
        #[arg(long, value_enum, default_value_t = OutputFormat::Summary)]
        format: OutputFormat,

        /// Write the full JSON report to this path and print telemetry to
        /// stdout.
        #[arg(long)]
        json_output: Option<PathBuf>,
    },

    /// Explain why a path or package selects tests and where selection widens.
    Explain {
        /// Changed repository-relative path to explain.
        #[arg(required_unless_present = "package", conflicts_with = "package")]
        path: Option<String>,

        /// Cargo package whose reverse-dependency selection should be
        /// explained.
        #[arg(long, conflicts_with = "path")]
        package: Option<String>,

        /// Repository or subdirectory root to analyze.
        #[arg(long, default_value = ".")]
        root: PathBuf,

        /// Current rkyv index output path.
        #[arg(long)]
        current_cache: Option<PathBuf>,

        /// Validate selected filters with cargo-nextest. This may compile
        /// repository code and execute build scripts; use only on trusted code.
        #[arg(long)]
        validate_nextest: bool,

        /// Output format for stdout.
        #[arg(long, value_enum, default_value_t = OutputFormat::Summary)]
        format: OutputFormat,

        /// Write the full JSON report to this path and print telemetry to
        /// stdout.
        #[arg(long)]
        json_output: Option<PathBuf>,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum OutputFormat {
    Json,
    Summary,
}

pub fn run() -> miette::Result<()> {
    let cli = Cli::parse();
    execute(cli).map_err(Into::into)
}

pub fn execute(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Index {
            root,
            cache,
            format,
            json_output,
        } => {
            let options = IndexOptions {
                root,
                cache_path: cache,
            };
            let (metadata, summary) = build_index(&options)?;

            let report = IndexReport {
                summary,
                metadata: Some(metadata),
            };
            write_json_output(json_output.as_deref(), &report)?;

            match (format, json_output.is_some()) {
                (OutputFormat::Summary, _) | (OutputFormat::Json, true) => {
                    print_index_telemetry(&report.summary)
                }
                (OutputFormat::Json, false) => print_json(&report)?,
            }
        }
        Commands::Affected {
            root,
            base_cache,
            current_cache,
            files,
            base_rev,
            head_rev,
            merge_base,
            worktree,
            staged,
            files_stdin,
            validate_nextest,
            format,
            json_output,
        } => {
            let changed_files = collect_changed_files(ChangeInputOptions {
                root: &root,
                files,
                files_stdin,
                base_rev,
                head_rev,
                merge_base,
                worktree,
                staged,
            })?;
            let options = AffectedOptions {
                root,
                files: Vec::new(),
                changed_files,
                base_cache: base_cache.or_else(default_base_cache),
                current_cache,
            };
            let report = if validate_nextest {
                compute_affected_validated(&options, &CommandFerrisRunner)?
            } else {
                compute_affected(&options, &CommandFerrisRunner)?
            };
            write_json_output(json_output.as_deref(), &report)?;

            match (format, json_output.is_some()) {
                (OutputFormat::Summary, _) | (OutputFormat::Json, true) => {
                    print_affected_summary(&report)
                }
                (OutputFormat::Json, false) => print_json(&report)?,
            }
        }
        Commands::Explain {
            path,
            package,
            root,
            current_cache,
            validate_nextest,
            format,
            json_output,
        } => {
            let report = if let Some(path) = path {
                let options = AffectedOptions {
                    root,
                    files: Vec::new(),
                    changed_files: vec![ChangedFileInput::path_only(path.clone())],
                    base_cache: None,
                    current_cache,
                };
                let affected = if validate_nextest {
                    compute_affected_validated(&options, &CommandFerrisRunner)?
                } else {
                    compute_affected(&options, &CommandFerrisRunner)?
                };
                ExplainReport::for_path(&path, affected)
            } else {
                let package = package.ok_or_else(|| LitmusError::InvalidArguments {
                    message: "explain requires a path or --package".to_string(),
                })?;
                let (metadata, _) = build_index(&IndexOptions {
                    root,
                    cache_path: current_cache,
                })?;
                ExplainReport::for_package(&metadata, &package)?
            };
            write_json_output(json_output.as_deref(), &report)?;
            match (format, json_output.is_some()) {
                (OutputFormat::Summary, _) | (OutputFormat::Json, true) => {
                    print_explain_summary(&report)
                }
                (OutputFormat::Json, false) => print_json(&report)?,
            }
        }
    }

    Ok(())
}

fn print_explain_summary(report: &ExplainReport) {
    println!(
        "cargo-litmus explain schema_version={}",
        report.schema_version
    );
    println!("subject={:?}", report.subject);
    println!("nextest_validation={:?}", report.nextest_validation);
    println!("failed_wide={}", report.failed_wide);
    for chain in &report.selection_chains {
        println!(
            "chain={} --{}--> {}",
            chain.from, chain.relationship, chain.to
        );
    }
    for reason in &report.widening_reasons {
        println!("widening_reason={reason}");
    }
}

fn write_json_output<T: Serialize>(path: Option<&Path>, report: &T) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| LitmusError::io(parent, source))?;
    }
    let rendered = serde_json::to_string_pretty(report)?;
    fs::write(path, rendered).map_err(|source| LitmusError::io(path, source))
}

struct ChangeInputOptions<'a> {
    root: &'a Path,
    files: Vec<String>,
    files_stdin: bool,
    base_rev: Option<String>,
    head_rev: Option<String>,
    merge_base: Option<String>,
    worktree: bool,
    staged: bool,
}

fn collect_changed_files(options: ChangeInputOptions<'_>) -> Result<Vec<ChangedFileInput>> {
    let ChangeInputOptions {
        root,
        files,
        files_stdin,
        base_rev,
        head_rev,
        merge_base,
        worktree,
        staged,
    } = options;
    let mut changed_files = files
        .into_iter()
        .map(ChangedFileInput::path_only)
        .collect::<Vec<_>>();
    if files_stdin {
        let mut raw = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw)
            .map_err(|source| crate::error::LitmusError::io("stdin", source))?;
        changed_files.extend(parse_changed_file_inputs(&raw));
    }
    if let (Some(base_rev), Some(head_rev)) = (base_rev, head_rev) {
        let revision_files = ChangeSet::revisions(root, &base_rev, &head_rev)?.files;
        changed_files = merge_revision_changed_files(changed_files, revision_files);
    } else if let Some(upstream) = merge_base {
        changed_files.extend(ChangeSet::merge_base(root, &upstream)?.files);
    } else if worktree {
        changed_files.extend(ChangeSet::worktree(root)?.files);
    } else if staged {
        changed_files.extend(ChangeSet::staged(root)?.files);
    }

    Ok(merge_changed_file_inputs(changed_files))
}

fn merge_revision_changed_files(
    explicit_files: Vec<ChangedFileInput>,
    revision_files: Vec<ChangedFileInput>,
) -> Vec<ChangedFileInput> {
    if explicit_files.is_empty() {
        return revision_files;
    }

    let explicit_paths = explicit_files
        .iter()
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>();
    explicit_files
        .into_iter()
        .chain(
            revision_files
                .into_iter()
                .filter(|file| explicit_paths.contains(&file.path)),
        )
        .collect()
}

fn default_base_cache() -> Option<PathBuf> {
    std::env::var_os("LITMUS_BASE_CACHE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn print_index_telemetry(summary: &impl Serialize) {
    let Ok(summary) = serde_json::to_value(summary) else {
        return;
    };
    println!("cargo-litmus indexed Rust source");
    for key in [
        "files_parsed",
        "items_indexed",
        "packages_indexed",
        "targets_indexed",
        "package_dependencies",
        "edges",
        "parse_failures",
        "source_bytes",
        "cache_bytes",
        "elapsed_ms",
        "cache_path",
        "git_head",
        "rust_file_list_hash",
        "tracked_file_list_hash",
        "cargo_metadata_hash",
    ] {
        if let Some(value) = summary.get(key) {
            println!("{key}: {}", value.as_str().unwrap_or(&value.to_string()));
        }
    }
}

fn print_affected_summary(report: &AffectedReport) {
    println!("cargo-litmus affected report");
    println!("schema_version: {}", report.schema_version);
    println!(
        "nextest_validation: {}",
        summary_value(&report.nextest_validation)
    );
    println!("cache_status: {}", summary_value(&report.cache_status));
    println!("failed_wide: {}", report.failed_wide);
    println!("selection_reason: {}", report.selection_reason);
    println!("changed_files: {}", report.changed_files.len());
    println!("changed_items: {}", report.changed_items.len());
    println!("unknown_files: {}", report.unknown_files.len());
    println!("affected_crates: {}", report.affected_crates.len());
    println!(
        "affected_test_crates: {}",
        report.affected_test_crates.len()
    );
    println!("test_failed_wide: {}", report.test_failed_wide);
    println!("test_selection_reason: {}", report.test_selection_reason);
    let missing_test_dependents = report
        .test_selection_checks
        .iter()
        .map(|check| check.missing_dependents.len())
        .sum::<usize>();
    println!("missing_test_dependents: {missing_test_dependents}");
    for check in &report.test_selection_checks {
        println!(
            "test_selection_check: {}/{} result={} expected={} selected={} missing={}",
            check.changed_package.workspace,
            check.changed_package.name,
            check.result,
            check.expected_dependents.len(),
            check.selected_dependents.len(),
            check.missing_dependents.len()
        );
    }
    let widened_module_commands = report
        .nextest_commands
        .iter()
        .filter(|command| command.widened_from == Some(crate::affected::NextestCommandMode::Module))
        .count();
    println!("widened_module_commands: {widened_module_commands}");
    println!(
        "module_test_commands: {}",
        report
            .nextest_commands
            .iter()
            .filter(|command| { command.mode == crate::affected::NextestCommandMode::Module })
            .count()
    );
    println!(
        "test_target_commands: {}",
        report
            .nextest_commands
            .iter()
            .filter(|command| { command.mode == crate::affected::NextestCommandMode::TestTarget })
            .count()
    );
    println!(
        "package_test_commands: {}",
        report
            .nextest_commands
            .iter()
            .filter(|command| { command.mode == crate::affected::NextestCommandMode::Package })
            .count()
    );
    println!(
        "workspace_wide_commands: {}",
        report
            .nextest_commands
            .iter()
            .filter(|command| {
                command.mode == crate::affected::NextestCommandMode::WorkspaceWide
            })
            .count()
    );
    let validated_test_count = report
        .nextest_commands
        .iter()
        .map(|command| command.matched_tests.len())
        .sum::<usize>();
    println!("validated_test_count: {validated_test_count}");
    if !report.selected_test_packages.is_empty() {
        let selected = report
            .selected_test_packages
            .iter()
            .map(|package| format!("{}/{}: {}", package.workspace, package.name, package.reason))
            .collect::<Vec<_>>()
            .join("; ");
        println!("selected_test_packages: {selected}");
    }
    println!("affected_workspaces: {}", report.affected_workspaces.len());
    let workspace_failed_wide = report
        .workspace_selections
        .iter()
        .filter(|selection| selection.failed_wide)
        .count();
    println!("workspace_failed_wide: {workspace_failed_wide}");
    let test_workspace_failed_wide = report
        .test_workspace_selections
        .iter()
        .filter(|selection| selection.failed_wide)
        .count();
    println!("test_workspace_failed_wide: {test_workspace_failed_wide}");
    let affected_workspace_names = report
        .affected_workspaces
        .iter()
        .map(|workspace| workspace.name.as_str())
        .collect::<Vec<_>>()
        .join(",");
    println!("affected_workspace_names: {affected_workspace_names}");
    if let Some(summary) = &report.current_index {
        println!("index_files: {}", summary.files_parsed);
        println!("index_items: {}", summary.items_indexed);
        println!("index_packages: {}", summary.packages_indexed);
        println!("index_targets: {}", summary.targets_indexed);
        println!(
            "index_package_dependencies: {}",
            summary.package_dependencies
        );
        println!("index_edges: {}", summary.edges);
        println!("index_source_bytes: {}", summary.source_bytes);
        println!("index_cache_bytes: {}", summary.cache_bytes);
        println!("index_elapsed_ms: {}", summary.elapsed_ms);
        println!("index_cache_path: {}", summary.cache_path);
        println!("index_mode: {}", summary_value(&summary.index_mode));
        println!("index_files_reused: {}", summary.files_reused);
        println!("index_files_reparsed: {}", summary.files_reparsed);
        println!("index_files_removed: {}", summary.files_removed);
        println!("index_files_added: {}", summary.files_added);
    }
}

fn summary_value(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn print_json<T: Serialize>(report: &T) -> Result<()> {
    let rendered = serde_json::to_string_pretty(report)?;
    println!("{rendered}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn parses_index_command() {
        let cli = Cli::parse_from([
            "cargo-litmus",
            "index",
            "--root",
            "crates/app",
            "--format",
            "json",
            "--cache",
            "target/base.rkyv",
            "--json-output",
            "target/litmus-index.json",
        ]);

        let Commands::Index {
            root,
            cache,
            format,
            json_output,
            ..
        } = cli.command
        else {
            panic!("expected index command");
        };
        assert_eq!(root, PathBuf::from("crates/app"));
        assert_eq!(cache, Some(PathBuf::from("target/base.rkyv")));
        assert_eq!(format, OutputFormat::Json);
        assert_eq!(json_output, Some(PathBuf::from("target/litmus-index.json")));
    }

    #[test]
    fn parses_affected_command() {
        let cli = Cli::parse_from([
            "cargo-litmus",
            "affected",
            "--root",
            "crates/app",
            "--base-rev",
            "HEAD~1",
            "--head-rev",
            "HEAD",
            "--base-cache",
            "target/base.rkyv",
            "--current-cache",
            "target/current.rkyv",
            "--files",
            "src/lib.rs",
            "src/main.rs",
            "--files-stdin",
            "--format",
            "json",
            "--json-output",
            "target/litmus-affected.json",
        ]);

        let Commands::Affected {
            root,
            files,
            base_rev,
            head_rev,
            files_stdin,
            format,
            json_output,
            base_cache,
            current_cache,
            ..
        } = cli.command
        else {
            panic!("expected affected command");
        };
        assert_eq!(root, PathBuf::from("crates/app"));
        assert_eq!(files, vec!["src/lib.rs", "src/main.rs"]);
        assert_eq!(base_rev, Some("HEAD~1".to_string()));
        assert_eq!(head_rev, Some("HEAD".to_string()));
        assert!(files_stdin);
        assert_eq!(format, OutputFormat::Json);
        assert_eq!(
            json_output,
            Some(PathBuf::from("target/litmus-affected.json"))
        );
        assert_eq!(base_cache, Some(PathBuf::from("target/base.rkyv")));
        assert_eq!(current_cache, Some(PathBuf::from("target/current.rkyv")));
    }

    #[test]
    fn explicit_changed_files_keep_only_matching_revision_ranges() {
        let merged = merge_changed_file_inputs(merge_revision_changed_files(
            vec![ChangedFileInput::path_only("src/lib.rs")],
            vec![
                ChangedFileInput {
                    path: "src/lib.rs".to_string(),
                    ranges: vec![crate::changed_files::ChangedLineRange {
                        start_line: 4,
                        end_line: 6,
                    }],
                },
                ChangedFileInput {
                    path: ".github/scripts/setup_ci_matrix.py".to_string(),
                    ranges: vec![crate::changed_files::ChangedLineRange {
                        start_line: 10,
                        end_line: 12,
                    }],
                },
            ],
        ));

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].path, "src/lib.rs");
        assert_eq!(merged[0].ranges.len(), 1);
        assert_eq!(merged[0].ranges[0].start_line, 4);
        assert_eq!(merged[0].ranges[0].end_line, 6);
    }

    #[test]
    fn rev_only_changed_files_use_full_revision_diff() {
        let merged = merge_revision_changed_files(
            Vec::new(),
            vec![
                ChangedFileInput::path_only("src/lib.rs"),
                ChangedFileInput::path_only(".github/scripts/setup_ci_matrix.py"),
            ],
        );

        assert_eq!(
            merged.into_iter().map(|file| file.path).collect::<Vec<_>>(),
            vec!["src/lib.rs", ".github/scripts/setup_ci_matrix.py"]
        );
    }

    #[test]
    fn parses_first_class_change_modes() {
        for flag in ["--worktree", "--staged"] {
            let cli = Cli::try_parse_from(["cargo-litmus", "affected", flag]).unwrap();
            let Commands::Affected {
                worktree, staged, ..
            } = cli.command
            else {
                panic!("expected affected command");
            };
            assert_eq!(worktree, flag == "--worktree");
            assert_eq!(staged, flag == "--staged");
        }
        let cli = Cli::try_parse_from(["cargo-litmus", "affected", "--merge-base", "origin/main"])
            .unwrap();
        let Commands::Affected { merge_base, .. } = cli.command else {
            panic!("expected affected command");
        };
        assert_eq!(merge_base.as_deref(), Some("origin/main"));
    }

    #[test]
    fn parses_explain_path_and_package() {
        let path = Cli::try_parse_from(["cargo-litmus", "explain", "src/lib.rs"]).unwrap();
        assert!(matches!(
            path.command,
            Commands::Explain { path: Some(path), package: None, .. } if path == "src/lib.rs"
        ));
        let package = Cli::try_parse_from([
            "cargo-litmus",
            "explain",
            "--package",
            "cargo-litmus",
            "--format",
            "json",
        ])
        .unwrap();
        assert!(matches!(
            package.command,
            Commands::Explain { path: None, package: Some(package), format: OutputFormat::Json, .. }
                if package == "cargo-litmus"
        ));
    }
}
