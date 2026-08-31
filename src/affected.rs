use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::changed_files::{ChangedFileInput, ChangedLineRange, merge_changed_file_inputs};
use crate::config::{ConfiguredInput, LitmusConfig, RuleSelection};
use crate::error::Result;
use crate::ferris::{AffectedCrate, AffectedWorkspace, FerrisAffectedReport, FerrisRunner};
use crate::indexer::{IndexOptions, IndexSummary, build_or_update_index};
use crate::model::{
    FileNode, INDEX_VERSION, IndexMetadata, ItemKind, ItemNode, PackageNode, SourceDependencyKind,
    SpanRange,
};
use crate::nextest::{CommandNextestRunner, NextestRunner};
use crate::storage::load_index;

const REASON_BUILD_FROM_FERRIS: &str = "ferris-wheel ripples selected affected packages";
const REASON_BUILD_FROM_FERRIS_WITH_UNCERTAINTY: &str =
    "ferris-wheel ripples selected affected packages with workspace-local uncertainty";
const REASON_TEST_MODULES: &str =
    "changed lines are inside Rust test modules; selecting module filters";
const REASON_TEST_TARGET: &str = "changed integration test files; selecting owning test targets";
const REASON_TEST_SOURCE_PACKAGE: &str =
    "changed files are test sources; selecting only owning package tests";
const REASON_TEST_FERRIS_DAG: &str =
    "ferris-wheel ripples selected test packages from the dependency DAG";
const REASON_TEST_BIN_PACKAGE: &str =
    "changed files are binary entrypoints; selecting only owning package tests";
const REASON_TEST_PACKAGE_METADATA: &str =
    "changed package metadata or build script files; selecting owning package tests";
const REASON_TEST_SELF_CHECK_FAILED: &str =
    "test package closure self-check failed; running full workspace tests";
const REASON_WORKSPACE_LOCAL_UNCERTAINTY: &str =
    "workspace-local test selection uncertainty; running full workspace tests";
const REASON_WORKSPACE_INDEX_UNCERTAINTY: &str =
    "one or more changed files in this workspace were not represented in the index";

#[derive(Clone, Debug)]
pub struct AffectedOptions {
    pub root: PathBuf,
    pub files: Vec<String>,
    pub changed_files: Vec<ChangedFileInput>,
    pub base_cache: Option<PathBuf>,
    pub current_cache: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStatus {
    NoBaseCache,
    Warm,
    ColdRebuild,
    IncompatibleBase,
    UnreadableBase,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeStatus {
    Added,
    Modified,
    Removed,
    UnchangedStructureModified,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: FileChangeStatus,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    pub before_byte_len: Option<u64>,
    pub after_byte_len: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemChangeStatus {
    Added,
    Modified,
    Removed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ChangedItem {
    pub path: String,
    pub status: ItemChangeStatus,
    pub before: Option<ItemSummary>,
    pub after: Option<ItemSummary>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ItemSummary {
    pub kind: ItemKind,
    pub name: String,
    pub visibility: String,
    pub span: SpanRange,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AffectedReport {
    pub schema_version: u32,
    pub nextest_validation: NextestValidation,
    pub configured_inputs: Vec<ConfiguredInput>,
    pub input_explanations: Vec<InputExplanation>,
    pub affected_crates: Vec<AffectedCrate>,
    pub affected_workspaces: Vec<AffectedWorkspace>,
    pub directly_affected_crates: Vec<String>,
    pub directly_affected_workspaces: Vec<AffectedWorkspace>,
    pub affected_test_crates: Vec<AffectedCrate>,
    pub directly_affected_test_crates: Vec<String>,
    pub selected_test_packages: Vec<PackageSelectionReason>,
    pub nextest_commands: Vec<NextestCommand>,
    pub test_selection_checks: Vec<TestSelectionCheck>,
    pub test_workspace_selections: Vec<WorkspaceSelection>,
    pub test_selection_reason: String,
    pub test_failed_wide: bool,
    pub workspace_selections: Vec<WorkspaceSelection>,
    pub changed_files: Vec<ChangedFile>,
    pub changed_items: Vec<ChangedItem>,
    pub unknown_files: Vec<String>,
    pub selection_reason: String,
    pub failed_wide: bool,
    pub current_index: Option<IndexSummary>,
    pub cache_status: CacheStatus,
    pub base_index: Option<IndexReference>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InputExplanation {
    pub path: String,
    pub kind: InputKind,
    pub reason: String,
    pub packages: Vec<String>,
    pub direct_packages: Vec<String>,
    pub dependency_chains: Vec<PackageDependencyChain>,
    pub cargo_changes: Vec<CargoChangeKind>,
    pub workspaces: Vec<String>,
    pub failed_wide: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CargoChangeKind {
    Features,
    TargetSpecificDependencies,
    Profiles,
    WorkspaceInheritance,
    ManifestOther,
    Lockfile,
    BuildScript,
    CargoConfig,
    RustToolchain,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackageDependencyChain {
    pub from_package: String,
    pub to_package: String,
    pub relationship: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    RustSource,
    CargoManifest,
    CargoLock,
    BuildScript,
    CargoConfig,
    RustToolchain,
    ConfiguredRepositoryInput,
    PathModule,
    IncludedSource,
    PackageInput,
    Uncertain,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NextestValidation {
    Skipped,
    Performed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NextestCommandMode {
    Module,
    TestTarget,
    Package,
    WorkspaceWide,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NextestCommand {
    pub workspace: String,
    pub args: Vec<String>,
    pub mode: NextestCommandMode,
    pub module_filter: Option<String>,
    pub widened_from: Option<NextestCommandMode>,
    pub matched_tests: Vec<String>,
    pub reason: String,
    pub changed_files: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PackageSelectionReason {
    pub name: String,
    pub workspace: String,
    pub reason: String,
    pub changed_files: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct PackageRef {
    pub name: String,
    pub workspace: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TestSelectionCheck {
    pub changed_package: PackageRef,
    pub expected_dependents: Vec<PackageRef>,
    pub selected_dependents: Vec<PackageRef>,
    pub missing_dependents: Vec<PackageRef>,
    pub result: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct IndexReference {
    pub version: u32,
    pub git_head: Option<String>,
    pub rust_file_list_hash: String,
    pub tracked_file_list_hash: String,
    pub cargo_metadata_hash: String,
    pub files: usize,
    pub items: usize,
    pub packages: usize,
    pub targets: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WorkspaceSelection {
    pub workspace: AffectedWorkspace,
    pub failed_wide: bool,
    pub selection_reason: String,
    pub unknown_files: Vec<String>,
}

#[derive(Clone, Debug)]
struct BaseIndex {
    metadata: Option<IndexMetadata>,
    reference: Option<IndexReference>,
    status: CacheStatus,
}

fn load_base_index(path: Option<&std::path::Path>) -> Option<BaseIndex> {
    let path = path?;
    if !path.exists() {
        return Some(BaseIndex {
            metadata: None,
            reference: None,
            status: CacheStatus::ColdRebuild,
        });
    }
    match load_index(path) {
        Ok(metadata) if metadata.version == INDEX_VERSION => Some(BaseIndex {
            reference: Some(IndexReference::from_metadata(&metadata)),
            metadata: Some(metadata),
            status: CacheStatus::Warm,
        }),
        Ok(metadata) => Some(BaseIndex {
            reference: Some(IndexReference::from_metadata(&metadata)),
            metadata: None,
            status: CacheStatus::IncompatibleBase,
        }),
        Err(_) => Some(BaseIndex {
            metadata: None,
            reference: None,
            status: CacheStatus::UnreadableBase,
        }),
    }
}

impl IndexReference {
    fn from_metadata(metadata: &IndexMetadata) -> Self {
        Self {
            version: metadata.version,
            git_head: metadata.git_head.clone(),
            rust_file_list_hash: metadata.rust_file_list_hash.clone(),
            tracked_file_list_hash: metadata.tracked_file_list_hash.clone(),
            cargo_metadata_hash: metadata.cargo_metadata_hash.clone(),
            files: metadata.files.len(),
            items: metadata.items.len(),
            packages: metadata.packages.len(),
            targets: metadata.targets.len(),
        }
    }
}

pub fn compute_affected(
    options: &AffectedOptions,
    runner: &impl FerrisRunner,
) -> Result<AffectedReport> {
    compute_affected_internal(options, runner, &CommandNextestRunner, false)
}

/// Compute affected tests and dynamically validate filters with cargo-nextest.
///
/// This may compile repository code and execute build scripts. Callers must
/// only enable it for trusted repositories.
pub fn compute_affected_validated(
    options: &AffectedOptions,
    runner: &impl FerrisRunner,
) -> Result<AffectedReport> {
    compute_affected_internal(options, runner, &CommandNextestRunner, true)
}

pub fn compute_affected_with_nextest(
    options: &AffectedOptions,
    runner: &impl FerrisRunner,
    nextest: &impl NextestRunner,
) -> Result<AffectedReport> {
    compute_affected_internal(options, runner, nextest, true)
}

fn compute_affected_internal(
    options: &AffectedOptions,
    runner: &impl FerrisRunner,
    nextest: &impl NextestRunner,
    validate_nextest: bool,
) -> Result<AffectedReport> {
    let base = load_base_index(options.base_cache.as_deref());
    let cache_status = base
        .as_ref()
        .map(|base| base.status.clone())
        .unwrap_or(CacheStatus::NoBaseCache);
    let base_reference = base.as_ref().and_then(|base| base.reference.clone());
    let changed_file_inputs = selected_changed_file_inputs(options);
    let current = match build_or_update_index(
        &IndexOptions {
            root: options.root.clone(),
            cache_path: options.current_cache.clone(),
        },
        &changed_file_inputs,
    ) {
        Ok((metadata, summary)) if summary.parse_failures == 0 => Some((metadata, summary)),
        Ok((metadata, summary)) => {
            return fail_wide(
                options,
                runner,
                Some((metadata, summary)),
                "current index had parse or access failures",
                cache_status,
                base_reference,
            );
        }
        Err(_) => {
            return fail_wide(
                options,
                runner,
                None,
                "current index could not be built",
                cache_status,
                base_reference,
            );
        }
    };

    let Some((current_metadata, current_summary)) = current.as_ref() else {
        return fail_wide(
            options,
            runner,
            None,
            "current index could not be built",
            cache_status,
            base_reference,
        );
    };
    let base_metadata = base
        .as_ref()
        .and_then(|base| base.metadata.as_ref())
        .filter(|metadata| metadata.cargo_metadata_hash == current_metadata.cargo_metadata_hash);
    let cache_status = match (
        cache_status,
        base.as_ref().and_then(|base| base.metadata.as_ref()),
        base_metadata,
    ) {
        (CacheStatus::Warm, Some(_), None) => CacheStatus::IncompatibleBase,
        (status, _, _) => status,
    };
    let selected_files = changed_file_inputs
        .iter()
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();
    if selected_files.is_empty() {
        return fail_wide(
            options,
            runner,
            current,
            "no changed files were provided or discovered",
            cache_status,
            base_reference,
        );
    }

    let mut diff = match base_metadata {
        Some(base_metadata) => diff_indexes(base_metadata, current_metadata, &selected_files),
        None => diff_current_index(current_metadata, &selected_files),
    };
    let mut ferris = match runner.ripples(&options.root, &selected_files) {
        Ok(ferris) => ferris,
        Err(_) => {
            return fail_wide_with_diff(
                options,
                runner,
                diff,
                Some(current_summary.clone()),
                "cargo ferris-wheel ripples failed",
                cache_status,
                base_reference,
            );
        }
    };
    let configured_inputs =
        LitmusConfig::load_optional(&options.root)?.matches(selected_files.iter().cloned())?;
    apply_configured_inputs(current_metadata, &mut ferris, &configured_inputs)?;
    let input_explanations = explain_inputs(
        &options.root,
        current_metadata,
        &mut ferris,
        &changed_file_inputs,
        &configured_inputs,
    );
    let mapped_paths = input_explanations
        .iter()
        .filter(|input| !input.failed_wide)
        .map(|input| input.path.as_str())
        .collect::<BTreeSet<_>>();
    diff.unknown_files
        .retain(|path| !mapped_paths.contains(path.as_str()));

    let mut workspace_uncertainty = match workspace_uncertainty(
        &ferris.affected_workspaces,
        &options.root,
        &diff.unknown_files,
    ) {
        Some(workspace_uncertainty) => workspace_uncertainty,
        None => {
            let mut report = fail_wide_with_diff(
                options,
                runner,
                diff,
                Some(current_summary.clone()),
                "one or more changed files could not be mapped to an affected workspace",
                cache_status,
                base_reference,
            )?;
            report.configured_inputs = configured_inputs;
            report.input_explanations = input_explanations;
            return Ok(report);
        }
    };
    for input in &configured_inputs {
        if input.selection == RuleSelection::Workspace {
            for workspace in &input.workspaces {
                workspace_uncertainty
                    .entry(workspace.clone())
                    .or_default()
                    .push(input.path.clone());
            }
        }
    }

    let reason = if workspace_uncertainty.is_empty() {
        REASON_BUILD_FROM_FERRIS
    } else {
        REASON_BUILD_FROM_FERRIS_WITH_UNCERTAINTY
    };
    let mut test_selection = if configured_inputs.is_empty() {
        select_test_crates(
            current_metadata,
            &ferris,
            &diff,
            &workspace_uncertainty,
            &changed_file_inputs,
        )
    } else {
        select_test_crates_with_configured_inputs(
            current_metadata,
            &ferris,
            &diff,
            &workspace_uncertainty,
            &changed_file_inputs,
            true,
        )
    };
    if validate_nextest {
        validate_nextest_commands(
            options,
            &ferris.affected_workspaces,
            &mut test_selection,
            nextest,
        );
    }
    apply_test_selection_self_checks(
        current_metadata,
        &ferris,
        &diff,
        &workspace_uncertainty,
        &mut test_selection,
    );

    Ok(report_from_ferris(ReportFromFerris {
        ferris,
        diff,
        reason,
        failed_wide: false,
        current_index: Some(current_summary.clone()),
        workspace_uncertainty,
        test_selection,
        cache_status,
        base_index: base_reference,
        nextest_validation: if validate_nextest {
            NextestValidation::Performed
        } else {
            NextestValidation::Skipped
        },
        configured_inputs,
        input_explanations,
    }))
}

fn apply_configured_inputs(
    metadata: &IndexMetadata,
    ferris: &mut FerrisAffectedReport,
    inputs: &[ConfiguredInput],
) -> Result<()> {
    for input in inputs {
        match input.selection {
            RuleSelection::Workspace => {
                for workspace_name in &input.workspaces {
                    let Some(package) = metadata
                        .packages
                        .iter()
                        .find(|package| package.workspace == *workspace_name)
                    else {
                        return Err(crate::error::LitmusError::Config {
                            path: options_config_path(),
                            message: format!(
                                "configuration entry {} matched {} but workspace \
                                 {workspace_name:?} was not discovered",
                                input.rule_index + 1,
                                input.path
                            ),
                        });
                    };
                    insert_affected_workspace(
                        ferris,
                        AffectedWorkspace {
                            name: workspace_name.clone(),
                            path: package.workspace_path.clone(),
                        },
                    );
                }
            }
            RuleSelection::Packages => {
                for package_name in &input.packages {
                    let packages = metadata
                        .packages
                        .iter()
                        .filter(|package| package.name == *package_name)
                        .collect::<Vec<_>>();
                    if packages.is_empty() {
                        return Err(crate::error::LitmusError::Config {
                            path: options_config_path(),
                            message: format!(
                                "configuration entry {} matched {} but package {package_name:?} \
                                 was not discovered",
                                input.rule_index + 1,
                                input.path
                            ),
                        });
                    }
                    for package in packages {
                        insert_package_closure(metadata, ferris, &package.id);
                    }
                }
            }
        }
    }
    ferris.affected_crates.sort_by(|left, right| {
        left.workspace
            .cmp(&right.workspace)
            .then_with(|| left.name.cmp(&right.name))
    });
    ferris.directly_affected_crates.sort();
    Ok(())
}

fn explain_inputs(
    root: &Path,
    metadata: &IndexMetadata,
    ferris: &mut FerrisAffectedReport,
    changed_files: &[ChangedFileInput],
    configured_inputs: &[ConfiguredInput],
) -> Vec<InputExplanation> {
    let indexed_paths = metadata
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    let mut explanations = Vec::new();
    for changed_file in changed_files {
        let path = &changed_file.path;
        let configured = configured_inputs
            .iter()
            .filter(|input| input.path == *path)
            .collect::<Vec<_>>();
        if !configured.is_empty() {
            let configured_package_names = configured
                .iter()
                .flat_map(|input| input.packages.iter().cloned())
                .collect::<BTreeSet<_>>();
            let configured_workspaces = configured
                .iter()
                .flat_map(|input| input.workspaces.iter().cloned())
                .collect::<BTreeSet<_>>();
            let direct_ids = metadata
                .packages
                .iter()
                .filter(|package| configured_package_names.contains(&package.name))
                .map(|package| package.id.clone())
                .collect::<BTreeSet<_>>();
            let selected_ids = direct_ids
                .iter()
                .flat_map(|id| reverse_package_closure(metadata, id))
                .collect::<BTreeSet<_>>();
            let selected_packages = metadata
                .packages
                .iter()
                .filter(|package| {
                    selected_ids.contains(&package.id)
                        || configured_workspaces.contains(&package.workspace)
                })
                .collect::<Vec<_>>();
            explanations.push(InputExplanation {
                path: path.clone(),
                kind: InputKind::ConfiguredRepositoryInput,
                reason: "matched .cargo-litmus.toml repository input rule".to_string(),
                packages: selected_packages
                    .iter()
                    .map(|package| package.name.clone())
                    .collect(),
                direct_packages: configured_package_names.into_iter().collect(),
                dependency_chains: reverse_dependency_chains(metadata, &direct_ids),
                cargo_changes: Vec::new(),
                workspaces: selected_packages
                    .iter()
                    .map(|package| package.workspace.clone())
                    .chain(configured_workspaces)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                failed_wide: false,
            });
            continue;
        }

        if let Some((kind, reason)) = cargo_input_kind(path) {
            let cargo_changes = cargo_change_kinds(root, changed_file, &kind);
            if matches!(kind, InputKind::CargoConfig | InputKind::RustToolchain) {
                explanations.push(InputExplanation {
                    path: path.clone(),
                    kind,
                    reason: reason.to_string(),
                    packages: Vec::new(),
                    direct_packages: Vec::new(),
                    dependency_chains: Vec::new(),
                    cargo_changes,
                    workspaces: Vec::new(),
                    failed_wide: true,
                });
                continue;
            }
            let package = package_for_path(metadata, path);
            if let Some(package) = package {
                insert_package_closure(metadata, ferris, &package.id);
            }
            explanations.push(input_explanation_for_packages(
                metadata,
                path,
                kind,
                reason,
                package,
                package.is_none(),
                cargo_changes,
            ));
            continue;
        }

        let source_dependencies = metadata
            .source_dependencies
            .iter()
            .filter(|dependency| dependency.target_path == *path)
            .collect::<Vec<_>>();
        let indexed = indexed_paths.contains(path.as_str());
        if indexed || !source_dependencies.is_empty() {
            let mut packages = source_dependencies
                .iter()
                .filter_map(|dependency| package_for_path(metadata, &dependency.source_path))
                .map(|package| (package.id.clone(), package))
                .collect::<BTreeMap<_, _>>();
            if let Some(package) = package_for_path(metadata, path) {
                packages.insert(package.id.clone(), package);
            }
            for package in packages.values() {
                insert_package_closure(metadata, ferris, &package.id);
            }
            let (kind, reason) = if source_dependencies
                .iter()
                .any(|dependency| dependency.kind == SourceDependencyKind::PathModule)
            {
                (
                    InputKind::PathModule,
                    "referenced by an indexed #[path] module",
                )
            } else if !source_dependencies.is_empty() {
                (
                    InputKind::IncludedSource,
                    "referenced by an indexed include macro",
                )
            } else {
                (
                    InputKind::RustSource,
                    "indexed Rust source maps to its owning package and reverse dependents",
                )
            };
            let failed_wide = packages.is_empty();
            explanations.push(input_explanation_for_packages(
                metadata,
                path,
                kind,
                reason,
                packages.into_values(),
                failed_wide,
                Vec::new(),
            ));
            continue;
        }

        if looks_generated_input(path) {
            explanations.push(InputExplanation {
                path: path.clone(),
                kind: InputKind::Uncertain,
                reason: "generated or proc-macro output has no statically indexed source \
                         relationship"
                    .to_string(),
                packages: Vec::new(),
                direct_packages: Vec::new(),
                dependency_chains: Vec::new(),
                cargo_changes: Vec::new(),
                workspaces: Vec::new(),
                failed_wide: true,
            });
            continue;
        }

        if let Some(package) = package_for_path(metadata, path) {
            insert_package_closure(metadata, ferris, &package.id);
            let (kind, reason) = if path.ends_with(".rs") {
                (
                    InputKind::RustSource,
                    "new or untracked Rust source is beneath a package root; selecting the \
                     package and reverse dependents",
                )
            } else {
                (
                    InputKind::PackageInput,
                    "non-Rust input is beneath a package root; selecting the package and reverse \
                     dependents",
                )
            };
            explanations.push(input_explanation_for_packages(
                metadata,
                path,
                kind,
                reason,
                std::iter::once(package),
                false,
                Vec::new(),
            ));
            continue;
        }

        explanations.push(InputExplanation {
            path: path.clone(),
            kind: InputKind::Uncertain,
            reason: "input is not indexed, configured, included, or beneath a known package root"
                .to_string(),
            packages: Vec::new(),
            direct_packages: Vec::new(),
            dependency_chains: Vec::new(),
            cargo_changes: Vec::new(),
            workspaces: Vec::new(),
            failed_wide: true,
        });
    }
    explanations.sort_by(|left, right| left.path.cmp(&right.path));
    ferris.affected_crates.sort_by(|left, right| {
        left.workspace
            .cmp(&right.workspace)
            .then_with(|| left.name.cmp(&right.name))
    });
    ferris.directly_affected_crates.sort();
    ferris.directly_affected_crates.dedup();
    explanations
}

fn looks_generated_input(path: &str) -> bool {
    path == "target"
        || path.starts_with("target/")
        || path.contains("/target/")
        || path.contains("/out/")
}

fn cargo_input_kind(path: &str) -> Option<(InputKind, &'static str)> {
    if path.ends_with("Cargo.toml") {
        Some((
            InputKind::CargoManifest,
            "Cargo manifest can change features, profiles, targets, dependencies, or workspace \
             membership",
        ))
    } else if path.ends_with("Cargo.lock") {
        Some((
            InputKind::CargoLock,
            "Cargo lockfile can change resolved transitive dependencies",
        ))
    } else if path.ends_with("build.rs") {
        Some((
            InputKind::BuildScript,
            "build script can consume undeclared inputs or change generated code",
        ))
    } else if path == ".cargo/config.toml"
        || path == ".cargo/config"
        || path.ends_with("/.cargo/config.toml")
        || path.ends_with("/.cargo/config")
    {
        Some((
            InputKind::CargoConfig,
            "Cargo configuration can change repository-wide builds, targets, rustflags, or runners",
        ))
    } else if path == "rust-toolchain.toml"
        || path == "rust-toolchain"
        || path.ends_with("/rust-toolchain.toml")
        || path.ends_with("/rust-toolchain")
    {
        Some((
            InputKind::RustToolchain,
            "Rust toolchain configuration can change compiler, component, and target behavior for \
             an entire workspace",
        ))
    } else {
        None
    }
}

fn cargo_change_kinds(
    root: &Path,
    changed_file: &ChangedFileInput,
    kind: &InputKind,
) -> Vec<CargoChangeKind> {
    match kind {
        InputKind::CargoManifest => {
            let contents = fs::read_to_string(root.join(&changed_file.path)).ok();
            manifest_change_kinds(contents.as_deref(), &changed_file.ranges)
        }
        InputKind::CargoLock => vec![CargoChangeKind::Lockfile],
        InputKind::BuildScript => vec![CargoChangeKind::BuildScript],
        InputKind::CargoConfig => vec![CargoChangeKind::CargoConfig],
        InputKind::RustToolchain => vec![CargoChangeKind::RustToolchain],
        _ => Vec::new(),
    }
}

fn manifest_change_kinds(
    contents: Option<&str>,
    ranges: &[ChangedLineRange],
) -> Vec<CargoChangeKind> {
    if ranges.is_empty() || contents.is_none() {
        return vec![
            CargoChangeKind::Features,
            CargoChangeKind::TargetSpecificDependencies,
            CargoChangeKind::Profiles,
            CargoChangeKind::WorkspaceInheritance,
            CargoChangeKind::ManifestOther,
        ];
    }
    let contents = contents.unwrap_or_default();
    let mut current_table = String::new();
    let mut kinds = BTreeSet::new();
    for (offset, line) in contents.lines().enumerate() {
        let line_number = offset + 1;
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            current_table = trimmed
                .trim_matches('[')
                .trim_matches(']')
                .trim()
                .to_string();
        }
        if !ranges
            .iter()
            .any(|range| range.start_line <= line_number && line_number <= range.end_line)
        {
            continue;
        }
        let kind = if current_table == "features"
            || current_table.ends_with(".features")
            || (trimmed.contains("features") && trimmed.contains('='))
        {
            CargoChangeKind::Features
        } else if current_table.starts_with("target.") && current_table.contains("dependencies") {
            CargoChangeKind::TargetSpecificDependencies
        } else if current_table == "profile" || current_table.starts_with("profile.") {
            CargoChangeKind::Profiles
        } else if current_table == "workspace"
            || current_table.starts_with("workspace.")
            || trimmed.contains("workspace = true")
        {
            CargoChangeKind::WorkspaceInheritance
        } else {
            CargoChangeKind::ManifestOther
        };
        kinds.insert(kind);
    }
    if kinds.is_empty() {
        kinds.insert(CargoChangeKind::ManifestOther);
    }
    kinds.into_iter().collect()
}

fn input_explanation_for_packages<'a>(
    metadata: &IndexMetadata,
    path: &str,
    kind: InputKind,
    reason: &str,
    direct_packages: impl IntoIterator<Item = &'a PackageNode>,
    failed_wide: bool,
    cargo_changes: Vec<CargoChangeKind>,
) -> InputExplanation {
    let direct_ids = direct_packages
        .into_iter()
        .map(|package| package.id.clone())
        .collect::<BTreeSet<_>>();
    let selected_ids = direct_ids
        .iter()
        .flat_map(|id| reverse_package_closure(metadata, id))
        .collect::<BTreeSet<_>>();
    let direct_packages = metadata
        .packages
        .iter()
        .filter(|package| direct_ids.contains(&package.id))
        .map(|package| package.name.clone())
        .collect();
    let dependency_chains = reverse_dependency_chains(metadata, &direct_ids);
    let selected = metadata
        .packages
        .iter()
        .filter(|package| selected_ids.contains(&package.id))
        .collect::<Vec<_>>();
    InputExplanation {
        path: path.to_string(),
        kind,
        reason: reason.to_string(),
        packages: selected
            .iter()
            .map(|package| package.name.clone())
            .collect(),
        direct_packages,
        dependency_chains,
        cargo_changes,
        workspaces: selected
            .iter()
            .map(|package| package.workspace.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        failed_wide,
    }
}

fn reverse_dependency_chains(
    metadata: &IndexMetadata,
    direct_ids: &BTreeSet<String>,
) -> Vec<PackageDependencyChain> {
    let names = metadata
        .packages
        .iter()
        .map(|package| (package.id.as_str(), package.name.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut chains = metadata
        .package_dependencies
        .iter()
        .filter(|dependency| {
            direct_ids.iter().any(|direct| {
                reverse_package_closure(metadata, direct).contains(&dependency.package_id)
                    && reverse_package_closure(metadata, direct)
                        .contains(&dependency.depends_on_package_id)
            })
        })
        .map(|dependency| PackageDependencyChain {
            from_package: names
                .get(dependency.depends_on_package_id.as_str())
                .copied()
                .unwrap_or(&dependency.depends_on_package_id)
                .to_string(),
            to_package: names
                .get(dependency.package_id.as_str())
                .copied()
                .unwrap_or(&dependency.package_id)
                .to_string(),
            relationship: format!("reverse Cargo {} dependency", dependency.kind),
        })
        .collect::<Vec<_>>();
    chains.sort_by(|left, right| {
        left.from_package
            .cmp(&right.from_package)
            .then_with(|| left.to_package.cmp(&right.to_package))
            .then_with(|| left.relationship.cmp(&right.relationship))
    });
    chains.dedup();
    chains
}

fn insert_package_closure(
    metadata: &IndexMetadata,
    ferris: &mut FerrisAffectedReport,
    direct_package_id: &str,
) {
    for package_id in reverse_package_closure(metadata, direct_package_id) {
        let Some(package) = metadata
            .packages
            .iter()
            .find(|package| package.id == package_id)
        else {
            continue;
        };
        let directly_affected = package.id == direct_package_id;
        let affected = AffectedCrate {
            name: package.name.clone(),
            workspace: package.workspace.clone(),
            is_directly_affected: directly_affected,
            is_standalone: false,
        };
        if let Some(existing) = ferris
            .affected_crates
            .iter_mut()
            .find(|krate| krate.name == affected.name && krate.workspace == affected.workspace)
        {
            existing.is_directly_affected |= directly_affected;
        } else {
            ferris.affected_crates.push(affected);
        }
        if directly_affected && !ferris.directly_affected_crates.contains(&package.name) {
            ferris.directly_affected_crates.push(package.name.clone());
        }
        insert_affected_workspace(
            ferris,
            AffectedWorkspace {
                name: package.workspace.clone(),
                path: package.workspace_path.clone(),
            },
        );
    }
}

fn insert_affected_workspace(ferris: &mut FerrisAffectedReport, workspace: AffectedWorkspace) {
    if !ferris
        .affected_workspaces
        .iter()
        .any(|existing| existing.name == workspace.name)
    {
        ferris.affected_workspaces.push(workspace.clone());
        ferris.affected_workspaces.sort();
    }
    if !ferris
        .directly_affected_workspaces
        .iter()
        .any(|existing| existing.name == workspace.name)
    {
        ferris.directly_affected_workspaces.push(workspace);
        ferris.directly_affected_workspaces.sort();
    }
}

fn options_config_path() -> PathBuf {
    PathBuf::from(crate::config::CONFIG_FILE_NAME)
}

fn workspace_uncertainty(
    affected_workspaces: &[AffectedWorkspace],
    root: &std::path::Path,
    unknown_files: &[String],
) -> Option<BTreeMap<String, Vec<String>>> {
    let mut unknown_files_by_workspace: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for unknown_file in unknown_files {
        let workspace = workspace_for_path(affected_workspaces, root, unknown_file)?;
        unknown_files_by_workspace
            .entry(workspace.name.clone())
            .or_default()
            .push(unknown_file.clone());
    }

    Some(unknown_files_by_workspace)
}

fn workspace_for_path<'a>(
    affected_workspaces: &'a [AffectedWorkspace],
    root: &std::path::Path,
    path: &str,
) -> Option<&'a AffectedWorkspace> {
    let normalized_path = normalize_path(path);
    affected_workspaces
        .iter()
        .filter_map(|workspace| {
            let workspace_path = repo_relative_workspace_path(root, &workspace.path);
            if path_is_in_workspace(&normalized_path, &workspace_path) {
                Some((workspace_path.len(), workspace))
            } else {
                None
            }
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, workspace)| workspace)
}

fn repo_relative_workspace_path(root: &std::path::Path, path: &str) -> String {
    let path = std::path::Path::new(path);
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let relative = if path.is_absolute() {
        path.strip_prefix(&root).unwrap_or(path)
    } else {
        path
    };
    normalize_path(&relative.to_string_lossy())
}

fn normalize_path(path: &str) -> String {
    path.trim()
        .trim_start_matches("./")
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string()
}

fn path_is_in_workspace(path: &str, workspace_path: &str) -> bool {
    workspace_path.is_empty()
        || workspace_path == "."
        || path == workspace_path
        || path.starts_with(&format!("{workspace_path}/"))
}

fn global_fail_wide_report(
    options: &AffectedOptions,
    runner: &impl FerrisRunner,
    diff: DiffResult,
    current_index: Option<IndexSummary>,
    reason: &str,
    cache_status: CacheStatus,
    base_index: Option<IndexReference>,
) -> Result<AffectedReport> {
    let ferris = runner.lineup(&options.root)?;
    let workspace_uncertainty = ferris
        .affected_workspaces
        .iter()
        .map(|workspace| (workspace.name.clone(), diff.unknown_files.clone()))
        .collect();
    let changed_file_paths = diff
        .changed_files
        .iter()
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();
    let test_selection = TestSelection::from_ferris(
        &ferris,
        reason,
        true,
        &workspace_uncertainty,
        &changed_file_paths,
    );
    Ok(report_from_ferris(ReportFromFerris {
        ferris,
        diff,
        reason,
        failed_wide: true,
        current_index,
        workspace_uncertainty,
        test_selection,
        cache_status,
        base_index,
        nextest_validation: NextestValidation::Skipped,
        configured_inputs: Vec::new(),
        input_explanations: Vec::new(),
    }))
}

fn fail_wide_with_diff(
    options: &AffectedOptions,
    runner: &impl FerrisRunner,
    diff: DiffResult,
    current_index: Option<IndexSummary>,
    reason: &str,
    cache_status: CacheStatus,
    base_index: Option<IndexReference>,
) -> Result<AffectedReport> {
    global_fail_wide_report(
        options,
        runner,
        diff,
        current_index,
        reason,
        cache_status,
        base_index,
    )
}

struct ReportFromFerris<'a> {
    ferris: FerrisAffectedReport,
    diff: DiffResult,
    reason: &'a str,
    failed_wide: bool,
    current_index: Option<IndexSummary>,
    workspace_uncertainty: BTreeMap<String, Vec<String>>,
    test_selection: TestSelection,
    cache_status: CacheStatus,
    base_index: Option<IndexReference>,
    nextest_validation: NextestValidation,
    configured_inputs: Vec<ConfiguredInput>,
    input_explanations: Vec<InputExplanation>,
}

fn report_from_ferris(input: ReportFromFerris<'_>) -> AffectedReport {
    let ReportFromFerris {
        ferris,
        diff,
        reason,
        failed_wide,
        current_index,
        workspace_uncertainty,
        test_selection,
        cache_status,
        base_index,
        nextest_validation,
        configured_inputs,
        input_explanations,
    } = input;
    let workspace_selections = workspace_selections(
        &ferris.affected_workspaces,
        &workspace_uncertainty,
        reason,
        failed_wide,
    );

    AffectedReport {
        schema_version: 1,
        nextest_validation,
        configured_inputs,
        input_explanations,
        affected_test_crates: test_selection.affected_test_crates,
        directly_affected_test_crates: test_selection.directly_affected_test_crates,
        selected_test_packages: test_selection.selected_test_packages,
        nextest_commands: test_selection.nextest_commands,
        test_selection_checks: test_selection.test_selection_checks,
        test_workspace_selections: test_selection.workspace_selections,
        test_selection_reason: test_selection.reason,
        test_failed_wide: test_selection.failed_wide,
        affected_crates: ferris.affected_crates,
        affected_workspaces: ferris.affected_workspaces,
        directly_affected_crates: ferris.directly_affected_crates,
        directly_affected_workspaces: ferris.directly_affected_workspaces,
        workspace_selections,
        changed_files: diff.changed_files,
        changed_items: diff.changed_items,
        unknown_files: diff.unknown_files,
        selection_reason: reason.to_string(),
        failed_wide,
        current_index,
        cache_status,
        base_index,
    }
}

fn workspace_selections(
    workspaces: &[AffectedWorkspace],
    workspace_uncertainty: &BTreeMap<String, Vec<String>>,
    selection_reason: &str,
    global_failed_wide: bool,
) -> Vec<WorkspaceSelection> {
    workspaces
        .iter()
        .map(|workspace| {
            let unknown_files = workspace_uncertainty
                .get(&workspace.name)
                .cloned()
                .unwrap_or_default();
            let failed_wide = global_failed_wide || !unknown_files.is_empty();
            let reason = if global_failed_wide {
                selection_reason
            } else if failed_wide {
                REASON_WORKSPACE_INDEX_UNCERTAINTY
            } else {
                REASON_BUILD_FROM_FERRIS
            };
            WorkspaceSelection {
                workspace: workspace.clone(),
                failed_wide,
                selection_reason: reason.to_string(),
                unknown_files,
            }
        })
        .collect()
}

#[derive(Clone, Debug)]
struct TestSelection {
    affected_test_crates: Vec<AffectedCrate>,
    directly_affected_test_crates: Vec<String>,
    selected_test_packages: Vec<PackageSelectionReason>,
    nextest_commands: Vec<NextestCommand>,
    test_selection_checks: Vec<TestSelectionCheck>,
    workspace_selections: Vec<WorkspaceSelection>,
    reason: String,
    failed_wide: bool,
}

impl TestSelection {
    fn from_ferris(
        ferris: &FerrisAffectedReport,
        reason: &str,
        failed_wide: bool,
        workspace_uncertainty: &BTreeMap<String, Vec<String>>,
        changed_files: &[String],
    ) -> Self {
        let workspace_selections = workspace_selections(
            &ferris.affected_workspaces,
            workspace_uncertainty,
            reason,
            failed_wide,
        );
        let mut nextest_commands = if failed_wide {
            workspace_wide_nextest_commands(&ferris.affected_workspaces, reason, changed_files)
        } else {
            package_nextest_commands(&ferris.affected_crates, reason, changed_files)
        };
        apply_workspace_wide_commands(
            &mut nextest_commands,
            &ferris.affected_workspaces,
            &workspace_selections,
        );

        Self {
            affected_test_crates: ferris.affected_crates.clone(),
            directly_affected_test_crates: ferris.directly_affected_crates.clone(),
            selected_test_packages: package_selection_reasons(
                &ferris.affected_crates,
                reason,
                changed_files,
            ),
            test_selection_checks: Vec::new(),
            nextest_commands,
            workspace_selections,
            reason: reason.to_string(),
            failed_wide,
        }
    }
}

fn select_test_crates(
    metadata: &IndexMetadata,
    ferris: &FerrisAffectedReport,
    diff: &DiffResult,
    workspace_uncertainty: &BTreeMap<String, Vec<String>>,
    changed_file_inputs: &[ChangedFileInput],
) -> TestSelection {
    select_test_crates_with_configured_inputs(
        metadata,
        ferris,
        diff,
        workspace_uncertainty,
        changed_file_inputs,
        false,
    )
}

fn select_test_crates_with_configured_inputs(
    metadata: &IndexMetadata,
    ferris: &FerrisAffectedReport,
    diff: &DiffResult,
    workspace_uncertainty: &BTreeMap<String, Vec<String>>,
    changed_file_inputs: &[ChangedFileInput],
    has_configured_inputs: bool,
) -> TestSelection {
    let known_changed_paths = diff
        .changed_files
        .iter()
        .filter(|file| !diff.unknown_files.contains(&file.path))
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    let known_changed_path_strings = known_changed_paths
        .iter()
        .map(|path| (*path).to_string())
        .collect::<Vec<_>>();
    let bin_entrypoint_packages = known_changed_paths
        .iter()
        .filter_map(|path| bin_entrypoint_package(metadata, path))
        .collect::<BTreeSet<_>>();
    let changed_packages = known_changed_paths
        .iter()
        .filter_map(|path| package_for_path(metadata, path))
        .map(|package| package.id.clone())
        .collect::<BTreeSet<_>>();
    let test_source_packages = known_changed_paths
        .iter()
        .filter(|path| is_test_source_path(metadata, path))
        .filter_map(|path| package_for_path(metadata, path))
        .map(|package| package.id.clone())
        .collect::<BTreeSet<_>>();
    let changed_production_packages = changed_production_package_ids(metadata, diff);
    let package_metadata_or_build_packages = known_changed_paths
        .iter()
        .filter(|path| is_package_metadata_or_build_file(metadata, path))
        .filter_map(|path| package_for_path(metadata, path))
        .map(|package| package.id.clone())
        .collect::<BTreeSet<_>>();
    if has_configured_inputs {
        return TestSelection::from_ferris(
            ferris,
            REASON_TEST_FERRIS_DAG,
            false,
            workspace_uncertainty,
            &known_changed_path_strings,
        );
    }
    let module_test_commands = module_test_commands(metadata, changed_file_inputs);
    if !module_test_commands.is_empty() {
        let module_package_ids = package_ids_for_commands(metadata, &module_test_commands);
        if module_test_commands.len() > max_module_commands(&module_package_ids) {
            return package_local_test_selection(
                metadata,
                ferris,
                &module_package_ids,
                workspace_uncertainty,
                REASON_TEST_MODULES,
                &known_changed_path_strings,
            );
        }
        return command_test_selection(
            metadata,
            ferris,
            workspace_uncertainty,
            module_test_commands,
            REASON_TEST_MODULES,
            &known_changed_path_strings,
        );
    }

    let test_target_commands = test_target_commands(metadata, changed_file_inputs);
    if !test_target_commands.is_empty()
        && all_known_changes_are_test_sources(metadata, changed_file_inputs)
    {
        return command_test_selection(
            metadata,
            ferris,
            workspace_uncertainty,
            test_target_commands,
            REASON_TEST_TARGET,
            &known_changed_path_strings,
        );
    }

    if !changed_packages.is_empty()
        && changed_packages == test_source_packages
        && changed_production_packages.is_empty()
    {
        return package_local_test_selection(
            metadata,
            ferris,
            &test_source_packages,
            workspace_uncertainty,
            REASON_TEST_SOURCE_PACKAGE,
            &known_changed_path_strings,
        );
    }

    if !changed_packages.is_empty() && changed_packages == package_metadata_or_build_packages {
        return package_local_test_selection(
            metadata,
            ferris,
            &package_metadata_or_build_packages,
            workspace_uncertainty,
            REASON_TEST_PACKAGE_METADATA,
            &known_changed_path_strings,
        );
    }

    let selection_is_only_bin_entrypoints =
        !changed_packages.is_empty() && changed_packages == bin_entrypoint_packages;
    if !selection_is_only_bin_entrypoints {
        // Do not infer that production behavior is package-local from item visibility
        // or an unchanged Rust signature. Private code can be reached through
        // public callers, and downstream tests can observe that behavior. Trait
        // implementations, macro expansion, build output, and generated code
        // make a syntax-only proof even less reliable.
        return TestSelection::from_ferris(
            ferris,
            REASON_TEST_FERRIS_DAG,
            false,
            workspace_uncertainty,
            &known_changed_path_strings,
        );
    }

    package_local_test_selection(
        metadata,
        ferris,
        &bin_entrypoint_packages,
        workspace_uncertainty,
        REASON_TEST_BIN_PACKAGE,
        &known_changed_path_strings,
    )
}

fn package_local_test_selection(
    metadata: &IndexMetadata,
    ferris: &FerrisAffectedReport,
    package_ids: &BTreeSet<String>,
    workspace_uncertainty: &BTreeMap<String, Vec<String>>,
    reason: &str,
    changed_files: &[String],
) -> TestSelection {
    let packages_by_id = metadata
        .packages
        .iter()
        .map(|package| (package.id.clone(), package))
        .collect::<BTreeMap<_, _>>();
    let directly_affected = package_ids
        .iter()
        .filter_map(|package_id| packages_by_id.get(package_id).copied())
        .map(affected_crate_for_package)
        .collect::<Vec<_>>();
    let directly_affected_test_crates = directly_affected
        .iter()
        .map(|krate| krate.name.clone())
        .collect();
    let workspace_coverage = workspace_coverage_for_crates(metadata, ferris, &directly_affected);
    let mut workspace_selections = workspace_selections(
        &workspace_coverage.all_workspaces,
        workspace_uncertainty,
        reason,
        false,
    );
    mark_missing_owned_workspaces_wide(
        &mut workspace_selections,
        &workspace_coverage.missing_owned_workspaces,
    );
    let mut nextest_commands = package_nextest_commands(&directly_affected, reason, changed_files);
    apply_workspace_wide_commands(
        &mut nextest_commands,
        &workspace_coverage.all_workspaces,
        &workspace_selections,
    );

    TestSelection {
        selected_test_packages: package_selection_reasons(
            &directly_affected,
            reason,
            changed_files,
        ),
        nextest_commands,
        test_selection_checks: Vec::new(),
        affected_test_crates: directly_affected,
        directly_affected_test_crates,
        workspace_selections,
        reason: reason.to_string(),
        failed_wide: false,
    }
}

fn command_test_selection(
    metadata: &IndexMetadata,
    ferris: &FerrisAffectedReport,
    workspace_uncertainty: &BTreeMap<String, Vec<String>>,
    mut commands: Vec<NextestCommand>,
    reason: &str,
    changed_files: &[String],
) -> TestSelection {
    let affected_test_crates = crates_for_commands(ferris, metadata, &commands);
    let workspace_coverage = workspace_coverage_for_crates(metadata, ferris, &affected_test_crates);
    let mut workspace_selections = workspace_selections(
        &workspace_coverage.all_workspaces,
        workspace_uncertainty,
        reason,
        false,
    );
    mark_missing_owned_workspaces_wide(
        &mut workspace_selections,
        &workspace_coverage.missing_owned_workspaces,
    );
    apply_workspace_wide_commands(
        &mut commands,
        &workspace_coverage.all_workspaces,
        &workspace_selections,
    );

    TestSelection {
        directly_affected_test_crates: affected_test_crates
            .iter()
            .map(|krate| krate.name.clone())
            .collect(),
        selected_test_packages: package_selection_reasons(
            &affected_test_crates,
            reason,
            changed_files,
        ),
        nextest_commands: commands,
        test_selection_checks: Vec::new(),
        affected_test_crates,
        workspace_selections,
        reason: reason.to_string(),
        failed_wide: false,
    }
}

struct WorkspaceCoverage {
    all_workspaces: Vec<AffectedWorkspace>,
    missing_owned_workspaces: Vec<AffectedWorkspace>,
}

fn workspace_coverage_for_crates(
    metadata: &IndexMetadata,
    ferris: &FerrisAffectedReport,
    crates: &[AffectedCrate],
) -> WorkspaceCoverage {
    let selected_workspaces = workspaces_for_crates(metadata, crates);
    let ferris_workspace_names = ferris
        .affected_workspaces
        .iter()
        .map(|workspace| workspace.name.clone())
        .collect::<BTreeSet<_>>();
    let missing_owned_workspaces = selected_workspaces
        .into_iter()
        .filter(|workspace| !ferris_workspace_names.contains(&workspace.name))
        .collect::<Vec<_>>();
    let all_workspaces = affected_workspaces_with_extra(
        &ferris.affected_workspaces,
        missing_owned_workspaces.clone(),
    );

    WorkspaceCoverage {
        all_workspaces,
        missing_owned_workspaces,
    }
}

fn mark_missing_owned_workspaces_wide(
    workspace_selections: &mut [WorkspaceSelection],
    missing_workspaces: &[AffectedWorkspace],
) {
    if missing_workspaces.is_empty() {
        return;
    }
    let missing_names = missing_workspaces
        .iter()
        .map(|workspace| workspace.name.clone())
        .collect::<BTreeSet<_>>();
    for selection in workspace_selections {
        if missing_names.contains(&selection.workspace.name) {
            selection.failed_wide = true;
            selection.selection_reason = "owning package workspace was not selected by ferris; \
                                          running full workspace tests"
                .to_string();
        }
    }
}

fn apply_test_selection_self_checks(
    metadata: &IndexMetadata,
    ferris: &FerrisAffectedReport,
    diff: &DiffResult,
    workspace_uncertainty: &BTreeMap<String, Vec<String>>,
    test_selection: &mut TestSelection,
) {
    if test_selection.reason != REASON_TEST_FERRIS_DAG
        && test_selection.reason != REASON_TEST_SOURCE_PACKAGE
    {
        return;
    }

    let production_package_ids = changed_production_package_ids(metadata, diff);
    if production_package_ids.is_empty() {
        return;
    }

    let packages_by_id = package_by_id(metadata);
    let packages_by_ref = package_by_ref(metadata);
    let selected = selected_test_package_refs(&test_selection.affected_test_crates);

    let mut checks = Vec::new();
    let mut missing_workspaces = BTreeSet::<AffectedWorkspace>::new();
    for package_id in production_package_ids {
        let Some(package) = packages_by_id.get(&package_id) else {
            continue;
        };
        let expected_dependents = expected_dependent_refs(metadata, &packages_by_id, &package_id);
        let selected_dependents = selected_dependent_refs(&expected_dependents, &selected);
        let missing_dependents = expected_dependents
            .difference(&selected_dependents)
            .cloned()
            .collect::<Vec<_>>();
        missing_workspaces.extend(missing_dependent_workspaces(
            &missing_dependents,
            &packages_by_ref,
        ));
        let result = if missing_dependents.is_empty() {
            "ok"
        } else {
            "missing_dependents"
        };
        checks.push(TestSelectionCheck {
            changed_package: package_ref_for_package(package),
            expected_dependents: expected_dependents.into_iter().collect(),
            selected_dependents: selected_dependents.into_iter().collect(),
            missing_dependents,
            result: result.to_string(),
        });
    }

    let missing_count = checks
        .iter()
        .map(|check| check.missing_dependents.len())
        .sum::<usize>();
    let wide_workspace_names = missing_dependent_workspace_names(&checks);
    test_selection.test_selection_checks = checks;
    if missing_count == 0 {
        return;
    }

    test_selection.reason = REASON_TEST_SELF_CHECK_FAILED.to_string();
    let changed_files = diff
        .changed_files
        .iter()
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();
    let self_check_workspaces = affected_workspaces_with_extra(
        &ferris.affected_workspaces,
        missing_workspaces.into_iter().collect(),
    );
    let mut workspace_selections = workspace_selections(
        &self_check_workspaces,
        workspace_uncertainty,
        REASON_TEST_FERRIS_DAG,
        false,
    );
    for selection in &mut workspace_selections {
        if wide_workspace_names.contains(&selection.workspace.name) {
            selection.failed_wide = true;
            selection.selection_reason = REASON_TEST_SELF_CHECK_FAILED.to_string();
        }
    }
    test_selection.failed_wide = workspace_selections
        .iter()
        .all(|selection| selection.failed_wide);

    let self_check_crates = crates_for_self_check(
        metadata,
        &test_selection.affected_test_crates,
        &workspace_selections,
    );
    let mut nextest_commands = package_nextest_commands_excluding_workspaces(
        &self_check_crates,
        REASON_TEST_FERRIS_DAG,
        &changed_files,
        &wide_workspace_names,
    );
    apply_workspace_wide_commands(
        &mut nextest_commands,
        &self_check_workspaces,
        &workspace_selections,
    );

    test_selection.nextest_commands = nextest_commands;
    test_selection.affected_test_crates = self_check_crates;
    test_selection.directly_affected_test_crates = test_selection
        .affected_test_crates
        .iter()
        .map(|krate| krate.name.clone())
        .collect();
    test_selection.selected_test_packages = package_selection_reasons(
        &test_selection.affected_test_crates,
        REASON_TEST_SELF_CHECK_FAILED,
        &changed_files,
    );
    test_selection.workspace_selections = workspace_selections;
}

fn missing_dependent_workspace_names(checks: &[TestSelectionCheck]) -> BTreeSet<String> {
    checks
        .iter()
        .flat_map(|check| check.missing_dependents.iter())
        .map(|package| package.workspace.clone())
        .collect()
}

fn crates_for_self_check(
    metadata: &IndexMetadata,
    selected_crates: &[AffectedCrate],
    workspace_selections: &[WorkspaceSelection],
) -> Vec<AffectedCrate> {
    let mut crates_by_key = selected_crates
        .iter()
        .map(|krate| ((krate.workspace.clone(), krate.name.clone()), krate.clone()))
        .collect::<BTreeMap<_, _>>();
    for selection in workspace_selections {
        if !selection.failed_wide {
            continue;
        }
        for krate in crates_for_workspaces(
            metadata,
            std::slice::from_ref(&selection.workspace),
            &selection.selection_reason,
        ) {
            crates_by_key.insert((krate.workspace.clone(), krate.name.clone()), krate);
        }
    }
    crates_by_key.into_values().collect()
}

fn package_nextest_commands_excluding_workspaces(
    crates: &[AffectedCrate],
    reason: &str,
    changed_files: &[String],
    workspace_names: &BTreeSet<String>,
) -> Vec<NextestCommand> {
    package_nextest_commands_for_crates(
        crates
            .iter()
            .filter(|krate| !workspace_names.contains(&krate.workspace)),
        reason,
        changed_files,
    )
}

fn package_by_id(metadata: &IndexMetadata) -> BTreeMap<String, &PackageNode> {
    metadata
        .packages
        .iter()
        .map(|package| (package.id.clone(), package))
        .collect()
}

fn package_by_ref(metadata: &IndexMetadata) -> BTreeMap<PackageRef, &PackageNode> {
    metadata
        .packages
        .iter()
        .map(|package| (package_ref_for_package(package), package))
        .collect()
}

fn selected_test_package_refs(crates: &[AffectedCrate]) -> BTreeSet<PackageRef> {
    crates
        .iter()
        .map(|krate| package_ref(krate.workspace.clone(), krate.name.clone()))
        .collect()
}

fn expected_dependent_refs(
    metadata: &IndexMetadata,
    packages_by_id: &BTreeMap<String, &PackageNode>,
    package_id: &str,
) -> BTreeSet<PackageRef> {
    reverse_package_closure(metadata, package_id)
        .into_iter()
        .filter_map(|id| {
            packages_by_id
                .get(&id)
                .map(|package| package_ref_for_package(package))
        })
        .collect()
}

fn selected_dependent_refs(
    expected_dependents: &BTreeSet<PackageRef>,
    selected_packages: &BTreeSet<PackageRef>,
) -> BTreeSet<PackageRef> {
    expected_dependents
        .intersection(selected_packages)
        .cloned()
        .collect()
}

fn missing_dependent_workspaces(
    missing_dependents: &[PackageRef],
    packages_by_ref: &BTreeMap<PackageRef, &PackageNode>,
) -> BTreeSet<AffectedWorkspace> {
    missing_dependents
        .iter()
        .filter_map(|missing| {
            packages_by_ref
                .get(missing)
                .map(|package| affected_workspace_for_package(package))
        })
        .collect()
}

fn affected_workspaces_with_extra(
    workspaces: &[AffectedWorkspace],
    extra: Vec<AffectedWorkspace>,
) -> Vec<AffectedWorkspace> {
    let mut seen = BTreeSet::new();
    let mut merged = Vec::new();
    for workspace in workspaces.iter().chain(extra.iter()) {
        if seen.insert((workspace.name.clone(), workspace.path.clone())) {
            merged.push(workspace.clone());
        }
    }
    merged
}

fn affected_workspace_for_package(package: &PackageNode) -> AffectedWorkspace {
    AffectedWorkspace {
        name: package.workspace.clone(),
        path: package.workspace_path.clone(),
    }
}

fn affected_crate_for_package(package: &PackageNode) -> AffectedCrate {
    AffectedCrate {
        name: package.name.clone(),
        workspace: package.workspace.clone(),
        is_directly_affected: true,
        is_standalone: package.workspace == package.name
            && package.workspace_path == package.root_path,
    }
}

fn workspaces_for_crates(
    metadata: &IndexMetadata,
    crates: &[AffectedCrate],
) -> Vec<AffectedWorkspace> {
    let selected = crates
        .iter()
        .map(|krate| (krate.workspace.clone(), krate.name.clone()))
        .collect::<BTreeSet<_>>();
    metadata
        .packages
        .iter()
        .filter(|package| selected.contains(&(package.workspace.clone(), package.name.clone())))
        .map(affected_workspace_for_package)
        .collect()
}

fn crates_for_workspaces(
    metadata: &IndexMetadata,
    workspaces: &[AffectedWorkspace],
    reason: &str,
) -> Vec<AffectedCrate> {
    let workspace_names = workspaces
        .iter()
        .map(|workspace| workspace.name.clone())
        .collect::<BTreeSet<_>>();
    metadata
        .packages
        .iter()
        .filter(|package| workspace_names.contains(&package.workspace))
        .map(|package| AffectedCrate {
            is_directly_affected: reason == REASON_TEST_SELF_CHECK_FAILED,
            ..affected_crate_for_package(package)
        })
        .collect()
}

fn changed_production_package_ids(metadata: &IndexMetadata, diff: &DiffResult) -> BTreeSet<String> {
    diff.changed_files
        .iter()
        .filter(|file| !diff.unknown_files.contains(&file.path))
        .filter_map(|file| {
            let package = package_for_path(metadata, &file.path)?;
            if is_test_source_path(metadata, &file.path)
                || bin_entrypoint_package(metadata, &file.path).as_deref() == Some(&package.id)
            {
                return None;
            }
            Some(package.id.clone())
        })
        .collect()
}

fn reverse_package_closure(metadata: &IndexMetadata, package_id: &str) -> BTreeSet<String> {
    let mut reverse_dependencies = BTreeMap::<String, Vec<String>>::new();
    for dependency in &metadata.package_dependencies {
        reverse_dependencies
            .entry(dependency.depends_on_package_id.clone())
            .or_default()
            .push(dependency.package_id.clone());
    }

    let mut visited = BTreeSet::from([package_id.to_string()]);
    let mut pending = vec![package_id.to_string()];
    while let Some(current) = pending.pop() {
        for dependent in reverse_dependencies.get(&current).into_iter().flatten() {
            if visited.insert(dependent.clone()) {
                pending.push(dependent.clone());
            }
        }
    }
    visited
}

fn package_ref_for_package(package: &PackageNode) -> PackageRef {
    package_ref(package.workspace.clone(), package.name.clone())
}

fn package_ref(workspace: String, name: String) -> PackageRef {
    PackageRef { name, workspace }
}

fn module_test_commands(
    metadata: &IndexMetadata,
    changed_files: &[ChangedFileInput],
) -> Vec<NextestCommand> {
    let mut modules = BTreeMap::<(String, String, String), Vec<String>>::new();
    for changed_file in changed_files {
        if changed_file.ranges.is_empty() {
            return Vec::new();
        }
        let Some(package) = package_for_path(metadata, &changed_file.path) else {
            return Vec::new();
        };
        if !is_package_src_path(package, &changed_file.path) {
            return Vec::new();
        }
        let Some(module_filters) =
            changed_test_module_filters(metadata, &changed_file.path, &changed_file.ranges)
        else {
            return Vec::new();
        };
        for module_filter in module_filters {
            modules
                .entry((
                    package.workspace.clone(),
                    package.name.clone(),
                    module_filter,
                ))
                .or_default()
                .push(changed_file.path.clone());
        }
    }

    modules
        .into_iter()
        .map(
            |((workspace, package, module_filter), changed_files)| NextestCommand {
                args: vec!["-p".to_string(), package, module_filter.clone()],
                ..nextest_command(
                    workspace,
                    NextestCommandMode::Module,
                    REASON_TEST_MODULES,
                    changed_files,
                    Some(module_filter),
                )
            },
        )
        .collect()
}

fn test_target_commands(
    metadata: &IndexMetadata,
    changed_files: &[ChangedFileInput],
) -> Vec<NextestCommand> {
    let mut commands = Vec::new();
    for changed_file in changed_files {
        let Some((package, target)) = test_target_for_path(metadata, &changed_file.path) else {
            continue;
        };
        let args = vec![
            "-p".to_string(),
            package.name.clone(),
            "--test".to_string(),
            target.name.clone(),
        ];
        commands.push(NextestCommand {
            args,
            ..nextest_command(
                package.workspace.clone(),
                NextestCommandMode::TestTarget,
                REASON_TEST_TARGET,
                vec![changed_file.path.clone()],
                None,
            )
        });
    }
    commands
}

fn nextest_command(
    workspace: String,
    mode: NextestCommandMode,
    reason: &str,
    changed_files: Vec<String>,
    module_filter: Option<String>,
) -> NextestCommand {
    NextestCommand {
        workspace,
        args: Vec::new(),
        mode,
        module_filter,
        widened_from: None,
        matched_tests: Vec::new(),
        reason: reason.to_string(),
        changed_files,
    }
}

fn changed_test_module_filters(
    metadata: &IndexMetadata,
    path: &str,
    ranges: &[ChangedLineRange],
) -> Option<Vec<String>> {
    if ranges.is_empty() {
        return None;
    }
    let file = files_by_path(metadata).get(path).copied()?;
    let mut selected = BTreeSet::new();
    for range in ranges {
        let module = metadata
            .items
            .iter()
            .filter(|item| item.file_id == file.id && item.is_test && item.kind == ItemKind::Mod)
            .filter(|item| item_fully_contains_range(item, range))
            .max_by_key(|item| item.name.len())?;
        selected.insert(module_filter(metadata, path, &module.name));
    }
    Some(selected.into_iter().collect())
}

fn item_fully_contains_range(item: &ItemNode, range: &ChangedLineRange) -> bool {
    item.span.start_line <= range.start_line && item.span.end_line >= range.end_line
}

fn module_filter(metadata: &IndexMetadata, path: &str, module_name: &str) -> String {
    let Some(package) = package_for_path(metadata, path) else {
        return module_name.to_string();
    };
    let src_prefix = format!("{}/src/", package.root_path);
    let Some(module_path) = path.strip_prefix(&src_prefix) else {
        return module_name.to_string();
    };
    let module_path = module_path.trim_end_matches(".rs");
    let module_path = if module_path == "lib" || module_path == "main" {
        ""
    } else {
        module_path
            .trim_end_matches("/mod")
            .trim_end_matches("/lib")
            .trim_end_matches("/main")
    }
    .replace('/', "::");
    if module_path.is_empty() {
        module_name.to_string()
    } else {
        format!("{module_path}::{module_name}")
    }
}

fn package_ids_for_commands(
    metadata: &IndexMetadata,
    commands: &[NextestCommand],
) -> BTreeSet<String> {
    command_package_names(commands)
        .into_iter()
        .filter_map(|(workspace, package_name)| {
            metadata
                .packages
                .iter()
                .find(|package| package.workspace == workspace && package.name == package_name)
                .map(|package| package.id.clone())
        })
        .collect()
}

fn command_package_names(commands: &[NextestCommand]) -> BTreeSet<(String, String)> {
    commands
        .iter()
        .filter_map(|command| {
            let package_name = package_arg(&command.args)?;
            Some((command.workspace.clone(), package_name))
        })
        .collect()
}

fn max_module_commands(package_ids: &BTreeSet<String>) -> usize {
    package_ids.len().max(1) * 8
}

fn all_known_changes_are_test_sources(
    metadata: &IndexMetadata,
    changed_files: &[ChangedFileInput],
) -> bool {
    !changed_files.is_empty()
        && changed_files
            .iter()
            .all(|file| is_test_source_path(metadata, &file.path))
}

fn crates_for_commands(
    ferris: &FerrisAffectedReport,
    metadata: &IndexMetadata,
    commands: &[NextestCommand],
) -> Vec<AffectedCrate> {
    let package_names = command_package_names(commands);
    let mut selected = ferris
        .affected_crates
        .iter()
        .filter(|krate| package_names.contains(&(krate.workspace.clone(), krate.name.clone())))
        .cloned()
        .collect::<Vec<_>>();
    let existing = selected
        .iter()
        .map(|krate| (krate.workspace.clone(), krate.name.clone()))
        .collect::<BTreeSet<_>>();
    selected.extend(package_names.into_iter().filter_map(|(workspace, name)| {
        if existing.contains(&(workspace.clone(), name.clone())) {
            return None;
        }
        metadata
            .packages
            .iter()
            .any(|package| package.workspace == workspace && package.name == name)
            .then_some(AffectedCrate {
                name,
                workspace,
                is_directly_affected: true,
                is_standalone: false,
            })
    }));
    selected
}

fn package_nextest_commands(
    crates: &[AffectedCrate],
    reason: &str,
    changed_files: &[String],
) -> Vec<NextestCommand> {
    package_nextest_commands_for_crates(crates.iter(), reason, changed_files)
}

fn package_nextest_commands_for_crates<'a>(
    crates: impl IntoIterator<Item = &'a AffectedCrate>,
    reason: &str,
    changed_files: &[String],
) -> Vec<NextestCommand> {
    let mut packages_by_workspace = BTreeMap::<String, BTreeSet<String>>::new();
    for krate in crates {
        packages_by_workspace
            .entry(krate.workspace.clone())
            .or_default()
            .insert(krate.name.clone());
    }

    packages_by_workspace
        .into_iter()
        .map(|(workspace, packages)| NextestCommand {
            args: packages
                .into_iter()
                .flat_map(|package| ["-p".to_string(), package])
                .collect(),
            ..nextest_command(
                workspace,
                NextestCommandMode::Package,
                reason,
                changed_files.to_vec(),
                None,
            )
        })
        .collect()
}

fn validate_nextest_commands(
    options: &AffectedOptions,
    workspaces: &[AffectedWorkspace],
    test_selection: &mut TestSelection,
    nextest: &impl NextestRunner,
) {
    let workspace_paths = workspaces
        .iter()
        .map(|workspace| (workspace.name.clone(), workspace.path.clone()))
        .collect::<BTreeMap<_, _>>();
    for command in &mut test_selection.nextest_commands {
        if command.mode != NextestCommandMode::Module {
            continue;
        }
        let Some(workspace_path) = workspace_paths.get(&command.workspace) else {
            widen_module_command(command);
            continue;
        };
        match nextest.list_tests(&options.root, workspace_path, &command.args) {
            Ok(matches) if !matches.is_empty() => command.matched_tests = matches,
            Ok(_) | Err(_) => widen_module_command(command),
        }
    }
    apply_workspace_wide_commands(
        &mut test_selection.nextest_commands,
        workspaces,
        &test_selection.workspace_selections,
    );
}

fn widen_module_command(command: &mut NextestCommand) {
    let Some(package) = package_arg(&command.args) else {
        command.args.clear();
        command.widened_from = Some(command.mode.clone());
        command.mode = NextestCommandMode::WorkspaceWide;
        command.module_filter = None;
        command.reason =
            "module filter validation failed; running full workspace tests".to_string();
        return;
    };
    command.args = vec!["-p".to_string(), package];
    command.widened_from = Some(command.mode.clone());
    command.mode = NextestCommandMode::Package;
    command.module_filter = None;
    command.matched_tests.clear();
    command.reason =
        "module filter validation matched no tests; widening to package tests".to_string();
}

fn package_arg(args: &[String]) -> Option<String> {
    args.windows(2)
        .find(|window| window[0] == "-p")
        .map(|window| window[1].clone())
}

fn apply_workspace_wide_commands(
    commands: &mut Vec<NextestCommand>,
    workspaces: &[AffectedWorkspace],
    selections: &[WorkspaceSelection],
) {
    let wide_workspaces = selections
        .iter()
        .filter(|selection| selection.failed_wide)
        .map(|selection| selection.workspace.name.clone())
        .collect::<BTreeSet<_>>();
    if wide_workspaces.is_empty() {
        return;
    }
    commands.retain(|command| !wide_workspaces.contains(&command.workspace));
    commands.extend(
        workspaces
            .iter()
            .filter(|workspace| wide_workspaces.contains(&workspace.name))
            .map(|workspace| {
                let selection = selections
                    .iter()
                    .find(|selection| selection.workspace.name == workspace.name);
                let changed_files = selection
                    .map(|selection| selection.unknown_files.clone())
                    .unwrap_or_default();
                let reason = selection
                    .map(|selection| selection.selection_reason.as_str())
                    .unwrap_or(REASON_WORKSPACE_LOCAL_UNCERTAINTY);
                nextest_command(
                    workspace.name.clone(),
                    NextestCommandMode::WorkspaceWide,
                    reason,
                    changed_files,
                    None,
                )
            }),
    );
}

fn workspace_wide_nextest_commands(
    workspaces: &[AffectedWorkspace],
    reason: &str,
    changed_files: &[String],
) -> Vec<NextestCommand> {
    workspaces
        .iter()
        .map(|workspace| {
            nextest_command(
                workspace.name.clone(),
                NextestCommandMode::WorkspaceWide,
                reason,
                changed_files.to_vec(),
                None,
            )
        })
        .collect()
}

fn package_selection_reasons(
    crates: &[AffectedCrate],
    reason: &str,
    changed_files: &[String],
) -> Vec<PackageSelectionReason> {
    crates
        .iter()
        .map(|krate| PackageSelectionReason {
            name: krate.name.clone(),
            workspace: krate.workspace.clone(),
            reason: reason.to_string(),
            changed_files: changed_files.to_vec(),
        })
        .collect()
}

fn bin_entrypoint_package(metadata: &IndexMetadata, path: &str) -> Option<String> {
    metadata
        .targets
        .iter()
        .find(|target| target.is_bin() && target.src_path == path)
        .map(|target| target.package_id.clone())
}

fn package_for_path<'a>(metadata: &'a IndexMetadata, path: &str) -> Option<&'a PackageNode> {
    metadata
        .packages
        .iter()
        .filter(|package| path_is_in_workspace(path, &package.root_path))
        .max_by_key(|package| package.root_path.len())
}

fn is_package_metadata_or_build_file(metadata: &IndexMetadata, path: &str) -> bool {
    package_for_path(metadata, path).is_some_and(|package| {
        path == package.manifest_path
            || path == format!("{}/Cargo.lock", package.root_path)
            || path == format!("{}/build.rs", package.root_path)
    })
}

fn is_package_src_path(package: &PackageNode, path: &str) -> bool {
    path == format!("{}/src.rs", package.root_path)
        || path.starts_with(&format!("{}/src/", package.root_path))
}

fn is_test_source_path(metadata: &IndexMetadata, path: &str) -> bool {
    metadata
        .targets
        .iter()
        .any(|target| target.is_test() && target.src_path == path)
        || package_for_path(metadata, path).is_some_and(|package| {
            path == format!("{}/tests.rs", package.root_path)
                || path.starts_with(&format!("{}/tests/", package.root_path))
                || path.contains(&format!("{}/tests/", package.root_path))
        })
}

fn test_target_for_path<'a>(
    metadata: &'a IndexMetadata,
    path: &str,
) -> Option<(&'a PackageNode, &'a crate::model::TargetNode)> {
    let target = metadata
        .targets
        .iter()
        .find(|target| target.is_test() && target.src_path == path)?;
    let package = metadata
        .packages
        .iter()
        .find(|package| package.id == target.package_id)?;
    Some((package, target))
}

fn fail_wide(
    options: &AffectedOptions,
    runner: &impl FerrisRunner,
    current: Option<(IndexMetadata, IndexSummary)>,
    reason: &str,
    cache_status: CacheStatus,
    base_index: Option<IndexReference>,
) -> Result<AffectedReport> {
    let selected_files = selected_changed_file_inputs(options)
        .into_iter()
        .map(|file| file.path)
        .collect::<Vec<_>>();
    let current_summary = current.as_ref().map(|(_, summary)| summary.clone());
    let diff = match current {
        Some((current, _)) => diff_current_index(&current, &selected_files),
        None => DiffResult {
            changed_files: Vec::new(),
            changed_items: Vec::new(),
            unknown_files: selected_files,
        },
    };
    fail_wide_with_diff(
        options,
        runner,
        diff,
        current_summary,
        reason,
        cache_status,
        base_index,
    )
}

fn selected_changed_file_inputs(options: &AffectedOptions) -> Vec<ChangedFileInput> {
    let mut files = options.changed_files.clone();
    files.extend(
        options
            .files
            .iter()
            .cloned()
            .map(ChangedFileInput::path_only),
    );
    merge_changed_file_inputs(files)
}

#[derive(Clone, Debug)]
pub struct DiffResult {
    changed_files: Vec<ChangedFile>,
    changed_items: Vec<ChangedItem>,
    unknown_files: Vec<String>,
}

pub fn diff_current_index(current: &IndexMetadata, changed_paths: &[String]) -> DiffResult {
    let current_files = files_by_path(current);

    let mut changed_files = Vec::new();
    let mut unknown_files = Vec::new();

    for path in changed_paths {
        match current_files.get(path) {
            Some(file) => {
                changed_files.push(ChangedFile {
                    path: path.clone(),
                    status: FileChangeStatus::Unknown,
                    before_hash: None,
                    after_hash: Some(file.hash.clone()),
                    before_byte_len: None,
                    after_byte_len: Some(file.byte_len),
                });
            }
            None => {
                if !is_package_metadata_or_build_file(current, path) {
                    unknown_files.push(path.clone());
                }
                changed_files.push(ChangedFile {
                    path: path.clone(),
                    status: FileChangeStatus::Unknown,
                    before_hash: None,
                    after_hash: None,
                    before_byte_len: None,
                    after_byte_len: None,
                });
            }
        }
    }

    DiffResult {
        changed_files,
        changed_items: Vec::new(),
        unknown_files,
    }
}

pub fn diff_indexes(
    before: &IndexMetadata,
    after: &IndexMetadata,
    changed_paths: &[String],
) -> DiffResult {
    let before_files = files_by_path(before);
    let after_files = files_by_path(after);
    let mut changed_files = Vec::new();
    let mut changed_items = Vec::new();
    let mut unknown_files = Vec::new();

    for path in changed_paths {
        let before_file = before_files.get(path).copied();
        let after_file = after_files.get(path).copied();
        let mut file_item_changes = diff_items_for_file(before, after, path);
        let status = match (before_file, after_file) {
            (None, None) => {
                if !is_package_metadata_or_build_file(before, path)
                    && !is_package_metadata_or_build_file(after, path)
                {
                    unknown_files.push(path.clone());
                }
                FileChangeStatus::Unknown
            }
            (None, Some(_)) => FileChangeStatus::Added,
            (Some(_), None) => FileChangeStatus::Removed,
            (Some(before), Some(after)) if before.hash == after.hash => {
                FileChangeStatus::UnchangedStructureModified
            }
            (Some(_), Some(_)) if file_item_changes.is_empty() => {
                FileChangeStatus::UnchangedStructureModified
            }
            (Some(_), Some(_)) => FileChangeStatus::Modified,
        };

        changed_items.append(&mut file_item_changes);
        changed_files.push(ChangedFile {
            path: path.clone(),
            status,
            before_hash: before_file.map(|file| file.hash.clone()),
            after_hash: after_file.map(|file| file.hash.clone()),
            before_byte_len: before_file.map(|file| file.byte_len),
            after_byte_len: after_file.map(|file| file.byte_len),
        });
    }

    DiffResult {
        changed_files,
        changed_items,
        unknown_files,
    }
}

fn diff_items_for_file(
    before: &IndexMetadata,
    after: &IndexMetadata,
    path: &str,
) -> Vec<ChangedItem> {
    let before_items = items_for_file_by_key(before, path);
    let after_items = items_for_file_by_key(after, path);
    let keys = before_items
        .keys()
        .chain(after_items.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut changes = Vec::new();

    for key in keys {
        match (before_items.get(&key), after_items.get(&key)) {
            (Some(before), Some(after))
                if before.visibility != after.visibility || before.span != after.span =>
            {
                changes.push(ChangedItem {
                    path: path.to_string(),
                    status: ItemChangeStatus::Modified,
                    before: Some(item_summary(before)),
                    after: Some(item_summary(after)),
                });
            }
            (Some(_), Some(_)) => {}
            (Some(before), None) => changes.push(ChangedItem {
                path: path.to_string(),
                status: ItemChangeStatus::Removed,
                before: Some(item_summary(before)),
                after: None,
            }),
            (None, Some(after)) => changes.push(ChangedItem {
                path: path.to_string(),
                status: ItemChangeStatus::Added,
                before: None,
                after: Some(item_summary(after)),
            }),
            (None, None) => {}
        }
    }

    changes
}

fn items_for_file_by_key<'a>(
    metadata: &'a IndexMetadata,
    path: &str,
) -> BTreeMap<(ItemKind, String), &'a ItemNode> {
    let files = files_by_path(metadata);
    let Some(file) = files.get(path) else {
        return BTreeMap::new();
    };
    metadata
        .items
        .iter()
        .filter(|item| item.file_id == file.id)
        .map(|item| ((item.kind.clone(), item.name.clone()), item))
        .collect()
}

fn item_summary(item: &ItemNode) -> ItemSummary {
    ItemSummary {
        kind: item.kind.clone(),
        name: item.name.clone(),
        visibility: item.visibility.clone(),
        span: item.span.clone(),
    }
}

fn files_by_path(metadata: &IndexMetadata) -> BTreeMap<String, &FileNode> {
    metadata
        .files
        .iter()
        .map(|file| (file.path.clone(), file))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::file_id;
    use crate::model::{
        Edge, INDEX_VERSION, ItemNode, PackageDependency, PackageNode, SpanRange, TargetNode,
    };
    use crate::nextest::NextestRunner;

    struct EmptyNextestRunner;

    impl NextestRunner for EmptyNextestRunner {
        fn list_tests(
            &self,
            _root: &std::path::Path,
            _workspace_path: &str,
            _args: &[String],
        ) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    fn metadata(path: &str, hash: &str) -> IndexMetadata {
        let file = FileNode {
            id: file_id(path),
            path: path.to_string(),
            hash: hash.to_string(),
            byte_len: hash.len() as u64,
        };
        IndexMetadata {
            version: INDEX_VERSION,
            tool_version: "test".to_string(),
            repo_root_hint: ".".to_string(),
            git_head: Some("head".to_string()),
            rust_file_list_hash: "rust".to_string(),
            tracked_file_list_hash: "tracked".to_string(),
            cargo_metadata_hash: "cargo".to_string(),
            generated_at_unix_ms: 0,
            files: vec![file],
            items: Vec::new(),
            packages: Vec::new(),
            targets: Vec::new(),
            package_dependencies: Vec::new(),
            module_declarations: Vec::new(),
            source_dependencies: Vec::new(),
            edges: Vec::<Edge>::new(),
        }
    }

    fn push_file(metadata: &mut IndexMetadata, path: &str) {
        metadata.files.push(FileNode {
            id: file_id(path),
            path: path.to_string(),
            hash: format!("hash:{path}"),
            byte_len: path.len() as u64,
        });
    }

    fn metadata_with_item(path: &str, hash: &str, name: &str, start_line: usize) -> IndexMetadata {
        let mut metadata = metadata(path, hash);
        let file_id = metadata.files[0].id.clone();
        metadata.items.push(ItemNode {
            id: format!("{path}:{name}:{start_line}"),
            file_id,
            kind: ItemKind::Fn,
            name: name.to_string(),
            visibility: "pub".to_string(),
            span: SpanRange {
                start_line,
                start_column: 1,
                end_line: start_line + 2,
                end_column: 1,
            },
            parent_item_id: None,
            is_test: false,
        });
        metadata
    }

    fn package_metadata(path: &str) -> IndexMetadata {
        let mut metadata = metadata(path, "hash");
        metadata.packages = vec![
            package_node("pkg-bin", "api-bin", "nodes", "api-bin"),
            package_node("pkg-lib", "api-lib", "nodes", "api-lib"),
        ];
        metadata.targets = vec![
            target_node(
                "target-bin",
                "pkg-bin",
                "api-bin",
                "bin",
                "api-bin/src/main.rs",
            ),
            target_node(
                "target-lib",
                "pkg-lib",
                "api_lib",
                "lib",
                "api-lib/src/lib.rs",
            ),
            target_node(
                "target-http-auth",
                "pkg-lib",
                "http_auth",
                "test",
                "api-lib/tests/the_tests/http_auth.rs",
            ),
        ];
        metadata.package_dependencies = vec![PackageDependency {
            package_id: "pkg-bin".to_string(),
            depends_on_package_id: "pkg-lib".to_string(),
            name: "api-lib".to_string(),
            depends_on_path: "api-lib".to_string(),
            kind: "normal".to_string(),
            target: None,
        }];
        metadata
    }

    fn push_test_module(
        metadata: &mut IndexMetadata,
        name: &str,
        start_line: usize,
        end_line: usize,
    ) {
        metadata.items.push(ItemNode {
            id: format!("test-module:{name}:{start_line}"),
            file_id: metadata.files[0].id.clone(),
            kind: ItemKind::Mod,
            name: name.to_string(),
            visibility: "private".to_string(),
            span: SpanRange {
                start_line,
                start_column: 1,
                end_line,
                end_column: 1,
            },
            parent_item_id: None,
            is_test: true,
        });
    }

    fn ferris_report() -> FerrisAffectedReport {
        FerrisAffectedReport {
            affected_crates: vec![
                affected_crate("nodes", "api-bin", true),
                affected_crate("nodes", "api-lib", false),
            ],
            affected_workspaces: vec![workspace("nodes")],
            directly_affected_crates: vec!["api-bin".to_string()],
            directly_affected_workspaces: vec![workspace("nodes")],
        }
    }

    fn ferris_report_for_api_lib_test() -> FerrisAffectedReport {
        FerrisAffectedReport {
            affected_crates: vec![
                affected_crate("nodes", "api-bin", false),
                affected_crate("nodes", "api-lib", true),
            ],
            affected_workspaces: vec![workspace("nodes")],
            directly_affected_crates: vec!["api-lib".to_string()],
            directly_affected_workspaces: vec![workspace("nodes")],
        }
    }

    fn ferris_report_without_api_bin_crate() -> FerrisAffectedReport {
        FerrisAffectedReport {
            affected_crates: vec![affected_crate("nodes", "api-lib", true)],
            affected_workspaces: vec![workspace("nodes")],
            directly_affected_crates: vec!["api-lib".to_string()],
            directly_affected_workspaces: vec![workspace("nodes")],
        }
    }

    fn ferris_report_without_nodes_workspace() -> FerrisAffectedReport {
        FerrisAffectedReport {
            affected_crates: vec![affected_crate("other", "other", true)],
            affected_workspaces: vec![workspace("other")],
            directly_affected_crates: vec!["other".to_string()],
            directly_affected_workspaces: vec![workspace("other")],
        }
    }

    fn cross_workspace_metadata() -> IndexMetadata {
        let mut metadata = metadata("core/math/src/lib.rs", "hash");
        metadata.packages = vec![
            package_node("pkg-math", "math", "core", "core/math"),
            package_node("pkg-state", "state", "sdk", "sdk/state"),
            package_node("pkg-api", "api", "nodes", "nodes/api"),
        ];
        metadata.package_dependencies = vec![
            PackageDependency {
                package_id: "pkg-state".to_string(),
                depends_on_package_id: "pkg-math".to_string(),
                name: "math".to_string(),
                depends_on_path: "core/math".to_string(),
                kind: "normal".to_string(),
                target: None,
            },
            PackageDependency {
                package_id: "pkg-api".to_string(),
                depends_on_package_id: "pkg-state".to_string(),
                name: "state".to_string(),
                depends_on_path: "sdk/state".to_string(),
                kind: "normal".to_string(),
                target: None,
            },
        ];
        metadata
    }

    fn package_node(id: &str, name: &str, workspace_name: &str, root_path: &str) -> PackageNode {
        PackageNode {
            id: id.to_string(),
            name: name.to_string(),
            workspace: workspace_name.to_string(),
            workspace_path: workspace_name.to_string(),
            manifest_path: format!("{root_path}/Cargo.toml"),
            root_path: root_path.to_string(),
        }
    }

    fn target_node(
        id: &str,
        package_id: &str,
        name: &str,
        kind: &str,
        src_path: &str,
    ) -> TargetNode {
        TargetNode {
            id: id.to_string(),
            package_id: package_id.to_string(),
            name: name.to_string(),
            kind: vec![kind.to_string()],
            test: true,
            src_path: src_path.to_string(),
        }
    }

    fn affected_crate(workspace: &str, name: &str, is_directly_affected: bool) -> AffectedCrate {
        AffectedCrate {
            name: name.to_string(),
            workspace: workspace.to_string(),
            is_directly_affected,
            is_standalone: false,
        }
    }

    fn workspace(name: &str) -> AffectedWorkspace {
        AffectedWorkspace {
            name: name.to_string(),
            path: name.to_string(),
        }
    }

    #[test]
    fn current_index_diff_reports_known_file_without_item_diagnostics() {
        let after = metadata("src/lib.rs", "after");

        let diff = diff_current_index(&after, &["src/lib.rs".to_string()]);

        assert_eq!(diff.changed_files[0].status, FileChangeStatus::Unknown);
        assert!(diff.unknown_files.is_empty());
        assert!(diff.changed_items.is_empty());
    }

    #[test]
    fn current_index_diff_reports_unknown_file() {
        let after = metadata("src/lib.rs", "after");

        let diff = diff_current_index(&after, &["Cargo.toml".to_string()]);

        assert_eq!(diff.changed_files[0].status, FileChangeStatus::Unknown);
        assert_eq!(diff.unknown_files, vec!["Cargo.toml"]);
        assert!(diff.changed_items.is_empty());
    }

    #[test]
    fn base_current_diff_reports_comment_only_file_change() {
        let before = metadata_with_item("src/lib.rs", "before", "root", 1);
        let after = metadata_with_item("src/lib.rs", "after", "root", 1);

        let diff = diff_indexes(&before, &after, &["src/lib.rs".to_string()]);

        assert_eq!(
            diff.changed_files[0].status,
            FileChangeStatus::UnchangedStructureModified
        );
        assert!(diff.changed_items.is_empty());
    }

    #[test]
    fn base_current_diff_reports_modified_item_span() {
        let before = metadata_with_item("src/lib.rs", "before", "root", 1);
        let after = metadata_with_item("src/lib.rs", "after", "root", 3);

        let diff = diff_indexes(&before, &after, &["src/lib.rs".to_string()]);

        assert_eq!(diff.changed_files[0].status, FileChangeStatus::Modified);
        assert_eq!(diff.changed_items.len(), 1);
        assert_eq!(diff.changed_items[0].status, ItemChangeStatus::Modified);
    }

    #[test]
    fn base_current_diff_reports_added_and_removed_files() {
        let before = metadata("src/old.rs", "old");
        let after = metadata("src/new.rs", "new");

        let diff = diff_indexes(
            &before,
            &after,
            &["src/old.rs".to_string(), "src/new.rs".to_string()],
        );

        assert_eq!(diff.changed_files[0].status, FileChangeStatus::Removed);
        assert_eq!(diff.changed_files[1].status, FileChangeStatus::Added);
    }

    #[test]
    fn bin_entrypoint_test_selection_stays_on_owning_package() {
        let metadata = package_metadata("api-bin/src/main.rs");
        let diff = diff_current_index(&metadata, &["api-bin/src/main.rs".to_string()]);
        let selection = select_test_crates(
            &metadata,
            &ferris_report(),
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[],
        );

        assert_eq!(selection.affected_test_crates.len(), 1);
        assert_eq!(selection.affected_test_crates[0].name, "api-bin");
    }

    #[test]
    fn bin_entrypoint_selection_uses_litmus_package_when_ferris_omits_crate() {
        let metadata = package_metadata("api-bin/src/main.rs");
        let diff = diff_current_index(&metadata, &["api-bin/src/main.rs".to_string()]);
        let selection = select_test_crates(
            &metadata,
            &ferris_report_without_api_bin_crate(),
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[],
        );

        assert_eq!(selection.affected_test_crates.len(), 1);
        assert_eq!(selection.affected_test_crates[0].name, "api-bin");
        assert_eq!(
            selection.nextest_commands[0].args,
            vec!["-p".to_string(), "api-bin".to_string()]
        );
    }

    #[test]
    fn package_local_selection_widens_when_ferris_omits_owning_workspace() {
        let metadata = package_metadata("api-bin/src/main.rs");
        let diff = diff_current_index(&metadata, &["api-bin/src/main.rs".to_string()]);
        let selection = select_test_crates(
            &metadata,
            &ferris_report_without_nodes_workspace(),
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[],
        );

        let nodes = selection
            .workspace_selections
            .iter()
            .find(|selection| selection.workspace.name == "nodes")
            .unwrap();
        assert!(nodes.failed_wide);
        assert!(
            selection
                .nextest_commands
                .iter()
                .any(|command| command.workspace == "nodes"
                    && command.mode == NextestCommandMode::WorkspaceWide)
        );
    }

    #[test]
    fn ferris_selection_emits_workspace_wide_command_for_local_uncertainty() {
        let ferris = ferris_report();
        let selection = TestSelection::from_ferris(
            &ferris,
            REASON_TEST_FERRIS_DAG,
            false,
            &BTreeMap::from([("nodes".to_string(), vec!["schema.json".to_string()])]),
            &["schema.json".to_string()],
        );

        assert_eq!(selection.nextest_commands.len(), 1);
        assert_eq!(
            selection.nextest_commands[0].mode,
            NextestCommandMode::WorkspaceWide
        );
        assert_eq!(selection.nextest_commands[0].workspace, "nodes");
    }

    #[test]
    fn package_manifest_change_selects_owning_package_tests() {
        let metadata = package_metadata("api-lib/src/lib.rs");
        let diff = diff_current_index(&metadata, &["api-lib/Cargo.toml".to_string()]);
        let selection = select_test_crates(
            &metadata,
            &ferris_report_for_api_lib_test(),
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[],
        );

        assert!(diff.unknown_files.is_empty());
        assert_eq!(selection.reason, REASON_TEST_PACKAGE_METADATA);
        assert_eq!(selection.affected_test_crates.len(), 1);
        assert_eq!(selection.affected_test_crates[0].name, "api-lib");
        assert_eq!(
            selection.nextest_commands[0].args,
            vec!["-p".to_string(), "api-lib".to_string()]
        );
    }

    #[test]
    fn package_lockfile_change_selects_owning_package_tests() {
        let metadata = package_metadata("api-lib/src/lib.rs");
        let diff = diff_current_index(&metadata, &["api-lib/Cargo.lock".to_string()]);
        let selection = select_test_crates(
            &metadata,
            &ferris_report_for_api_lib_test(),
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[],
        );

        assert!(diff.unknown_files.is_empty());
        assert_eq!(selection.reason, REASON_TEST_PACKAGE_METADATA);
        assert_eq!(selection.affected_test_crates.len(), 1);
        assert_eq!(selection.affected_test_crates[0].name, "api-lib");
    }

    #[test]
    fn build_script_change_selects_owning_package_tests() {
        let metadata = package_metadata("api-lib/build.rs");
        let diff = diff_current_index(&metadata, &["api-lib/build.rs".to_string()]);
        let selection = select_test_crates(
            &metadata,
            &ferris_report_for_api_lib_test(),
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[],
        );

        assert!(diff.unknown_files.is_empty());
        assert_eq!(selection.reason, REASON_TEST_PACKAGE_METADATA);
        assert_eq!(selection.affected_test_crates.len(), 1);
        assert_eq!(selection.affected_test_crates[0].name, "api-lib");
    }

    #[test]
    fn command_selection_widens_when_ferris_omits_owning_workspace() {
        let mut metadata = package_metadata("api-lib/src/lib.rs");
        push_test_module(&mut metadata, "tests", 10, 20);
        let diff = diff_current_index(&metadata, &["api-lib/src/lib.rs".to_string()]);
        let selection = select_test_crates(
            &metadata,
            &ferris_report_without_nodes_workspace(),
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[ChangedFileInput {
                path: "api-lib/src/lib.rs".to_string(),
                ranges: vec![ChangedLineRange {
                    start_line: 12,
                    end_line: 12,
                }],
            }],
        );

        let nodes = selection
            .workspace_selections
            .iter()
            .find(|selection| selection.workspace.name == "nodes")
            .unwrap();
        assert!(nodes.failed_wide);
        assert!(
            selection
                .nextest_commands
                .iter()
                .any(|command| command.workspace == "nodes"
                    && command.mode == NextestCommandMode::WorkspaceWide)
        );
    }

    #[test]
    fn production_implementation_change_keeps_conservative_dependency_selection() {
        let metadata = package_metadata("api-lib/src/lib.rs");
        let diff = diff_current_index(&metadata, &["api-lib/src/lib.rs".to_string()]);
        let selection = select_test_crates(
            &metadata,
            &ferris_report(),
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[],
        );

        let names = selection
            .affected_test_crates
            .iter()
            .map(|krate| krate.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["api-bin", "api-lib"]);
        assert_eq!(selection.reason, REASON_TEST_FERRIS_DAG);
        assert_eq!(selection.nextest_commands.len(), 1);
        assert_eq!(
            selection.nextest_commands[0].args,
            vec!["-p", "api-bin", "-p", "api-lib"]
        );
    }

    #[test]
    fn production_and_test_changes_in_same_package_select_dependents() {
        let mut metadata = package_metadata("api-lib/src/lib.rs");
        push_file(&mut metadata, "api-lib/tests/the_tests/http_auth.rs");
        let changed_paths = vec![
            "api-lib/src/lib.rs".to_string(),
            "api-lib/tests/the_tests/http_auth.rs".to_string(),
        ];
        let diff = diff_current_index(&metadata, &changed_paths);
        let changed_files = changed_paths
            .iter()
            .cloned()
            .map(ChangedFileInput::path_only)
            .collect::<Vec<_>>();
        let selection = select_test_crates(
            &metadata,
            &ferris_report_for_api_lib_test(),
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &changed_files,
        );

        let names = selection
            .affected_test_crates
            .iter()
            .map(|krate| krate.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["api-bin", "api-lib"]);
        assert_eq!(selection.reason, REASON_TEST_FERRIS_DAG);
    }

    #[test]
    fn mixed_changes_in_test_touched_packages_use_dependency_selection() {
        let mut metadata = package_metadata("api-lib/src/lib.rs");
        for path in [
            "api-lib/tests/the_tests/http_auth.rs",
            "api-bin/src/commands.rs",
            "api-bin/tests/cli.rs",
        ] {
            push_file(&mut metadata, path);
        }
        let changed_paths = vec![
            "api-lib/src/lib.rs".to_string(),
            "api-lib/tests/the_tests/http_auth.rs".to_string(),
            "api-bin/src/commands.rs".to_string(),
            "api-bin/tests/cli.rs".to_string(),
        ];
        let diff = diff_current_index(&metadata, &changed_paths);
        let changed_files = changed_paths
            .iter()
            .cloned()
            .map(ChangedFileInput::path_only)
            .collect::<Vec<_>>();
        let selection = select_test_crates(
            &metadata,
            &ferris_report_for_api_lib_test(),
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &changed_files,
        );

        assert_eq!(selection.reason, REASON_TEST_FERRIS_DAG);
    }

    #[test]
    fn package_commands_are_consolidated_without_crossing_workspaces() {
        let crates = vec![
            affected_crate("nodes", "db", false),
            affected_crate("tools", "zero-test-bin", false),
            affected_crate("nodes", "api", true),
            affected_crate("nodes", "db", false),
        ];

        let commands = package_nextest_commands(&crates, "reason", &["src/lib.rs".to_string()]);

        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].workspace, "nodes");
        assert_eq!(commands[0].args, vec!["-p", "api", "-p", "db"]);
        assert_eq!(commands[0].mode, NextestCommandMode::Package);
        assert_eq!(commands[1].workspace, "tools");
        assert_eq!(commands[1].args, vec!["-p", "zero-test-bin"]);
    }

    #[test]
    fn package_command_consolidation_respects_wide_workspaces() {
        let crates = vec![
            affected_crate("nodes", "api", true),
            affected_crate("nodes", "db", false),
            affected_crate("tools", "cli", false),
        ];

        let commands = package_nextest_commands_excluding_workspaces(
            &crates,
            "reason",
            &[],
            &BTreeSet::from(["nodes".to_string()]),
        );

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].workspace, "tools");
        assert_eq!(commands[0].args, vec!["-p", "cli"]);
    }

    #[test]
    fn production_self_check_accepts_complete_reverse_package_closure() {
        let metadata = package_metadata("api-lib/src/lib.rs");
        let diff = diff_current_index(&metadata, &["api-lib/src/lib.rs".to_string()]);
        let ferris = ferris_report();
        let mut selection = select_test_crates(
            &metadata,
            &ferris,
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[],
        );

        apply_test_selection_self_checks(
            &metadata,
            &ferris,
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &mut selection,
        );

        assert!(!selection.failed_wide);
        assert_eq!(selection.test_selection_checks.len(), 1);
        assert_eq!(selection.test_selection_checks[0].result, "ok");
        let expected = selection.test_selection_checks[0]
            .expected_dependents
            .iter()
            .map(|package| package.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(expected, vec!["api-bin", "api-lib"]);
    }

    #[test]
    fn production_self_check_widens_when_dependent_package_is_missing() {
        let metadata = package_metadata("api-lib/src/lib.rs");
        let diff = diff_current_index(&metadata, &["api-lib/src/lib.rs".to_string()]);
        let mut ferris = ferris_report_for_api_lib_test();
        ferris
            .affected_crates
            .retain(|krate| krate.name == "api-lib");
        let mut selection = select_test_crates(
            &metadata,
            &ferris,
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[],
        );

        apply_test_selection_self_checks(
            &metadata,
            &ferris,
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &mut selection,
        );

        assert!(selection.failed_wide);
        assert_eq!(selection.reason, REASON_TEST_SELF_CHECK_FAILED);
        assert_eq!(selection.nextest_commands.len(), 1);
        assert_eq!(
            selection.nextest_commands[0].mode,
            NextestCommandMode::WorkspaceWide
        );
        assert_eq!(
            selection.test_selection_checks[0].missing_dependents,
            vec![PackageRef {
                name: "api-bin".to_string(),
                workspace: "nodes".to_string(),
            }]
        );
        let selected_names = selection
            .affected_test_crates
            .iter()
            .map(|krate| krate.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(selected_names, vec!["api-bin", "api-lib"]);
        assert_eq!(selection.selected_test_packages.len(), 2);
    }

    #[test]
    fn production_self_check_rejects_test_source_shortcut_bypassing_dependents() {
        let mut metadata = package_metadata("api-lib/src/lib.rs");
        push_file(&mut metadata, "api-lib/tests/the_tests/http_auth.rs");
        let diff = diff_current_index(
            &metadata,
            &[
                "api-lib/src/lib.rs".to_string(),
                "api-lib/tests/the_tests/http_auth.rs".to_string(),
            ],
        );
        let ferris = ferris_report_for_api_lib_test();
        let mut selection = package_local_test_selection(
            &metadata,
            &ferris,
            &BTreeSet::from(["pkg-lib".to_string()]),
            &BTreeMap::<String, Vec<String>>::new(),
            REASON_TEST_SOURCE_PACKAGE,
            &[],
        );

        apply_test_selection_self_checks(
            &metadata,
            &ferris,
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &mut selection,
        );

        assert_eq!(selection.reason, REASON_TEST_SELF_CHECK_FAILED);
        assert_eq!(
            selection.test_selection_checks[0].missing_dependents,
            vec![PackageRef {
                name: "api-bin".to_string(),
                workspace: "nodes".to_string(),
            }]
        );
    }

    #[test]
    fn production_self_check_catches_missing_cross_workspace_dependents() {
        let metadata = cross_workspace_metadata();
        let diff = diff_current_index(&metadata, &["core/math/src/lib.rs".to_string()]);
        let ferris = FerrisAffectedReport {
            affected_crates: vec![affected_crate("core", "math", true)],
            affected_workspaces: vec![workspace("core"), workspace("sdk"), workspace("nodes")],
            directly_affected_crates: vec!["math".to_string()],
            directly_affected_workspaces: vec![workspace("core")],
        };
        let mut selection = select_test_crates(
            &metadata,
            &ferris,
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[],
        );

        apply_test_selection_self_checks(
            &metadata,
            &ferris,
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &mut selection,
        );

        assert!(!selection.failed_wide);
        let missing = selection.test_selection_checks[0]
            .missing_dependents
            .iter()
            .map(|package| format!("{}/{}", package.workspace, package.name))
            .collect::<Vec<_>>();
        assert_eq!(missing, vec!["nodes/api", "sdk/state"]);
        let commands = selection
            .nextest_commands
            .iter()
            .map(|command| (command.workspace.as_str(), &command.mode))
            .collect::<Vec<_>>();
        assert_eq!(
            commands,
            vec![
                ("core", &NextestCommandMode::Package),
                ("sdk", &NextestCommandMode::WorkspaceWide),
                ("nodes", &NextestCommandMode::WorkspaceWide),
            ]
        );
    }

    #[test]
    fn test_only_changes_stay_on_owning_package() {
        let metadata = package_metadata("api-lib/tests/the_tests/http_auth.rs");
        let diff = diff_current_index(
            &metadata,
            &["api-lib/tests/the_tests/http_auth.rs".to_string()],
        );
        let selection = select_test_crates(
            &metadata,
            &ferris_report_for_api_lib_test(),
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[],
        );

        assert_eq!(selection.affected_test_crates.len(), 1);
        assert_eq!(selection.affected_test_crates[0].name, "api-lib");
        assert_eq!(selection.reason, REASON_TEST_SOURCE_PACKAGE);
    }

    #[test]
    fn changed_lines_inside_src_test_module_select_module_filter() {
        let mut metadata = package_metadata("api-lib/src/lib.rs");
        push_test_module(&mut metadata, "tests", 10, 20);
        let diff = diff_current_index(&metadata, &["api-lib/src/lib.rs".to_string()]);
        let selection = select_test_crates(
            &metadata,
            &ferris_report_for_api_lib_test(),
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[ChangedFileInput {
                path: "api-lib/src/lib.rs".to_string(),
                ranges: vec![ChangedLineRange {
                    start_line: 12,
                    end_line: 12,
                }],
            }],
        );

        assert_eq!(selection.nextest_commands.len(), 1);
        assert_eq!(selection.affected_test_crates.len(), 1);
        assert_eq!(selection.affected_test_crates[0].name, "api-lib");
        assert_eq!(
            selection.nextest_commands[0].mode,
            NextestCommandMode::Module
        );
        assert_eq!(
            selection.nextest_commands[0].module_filter.as_deref(),
            Some("tests")
        );
        assert_eq!(
            selection.nextest_commands[0].args,
            vec!["-p", "api-lib", "tests"]
        );
    }

    #[test]
    fn changed_lines_inside_nested_src_test_module_select_nested_module_filter() {
        let mut metadata = package_metadata("api-lib/src/http.rs");
        push_test_module(&mut metadata, "tests", 10, 20);
        let diff = diff_current_index(&metadata, &["api-lib/src/http.rs".to_string()]);
        let selection = select_test_crates(
            &metadata,
            &ferris_report_for_api_lib_test(),
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[ChangedFileInput {
                path: "api-lib/src/http.rs".to_string(),
                ranges: vec![ChangedLineRange {
                    start_line: 12,
                    end_line: 12,
                }],
            }],
        );

        assert_eq!(selection.nextest_commands.len(), 1);
        assert_eq!(
            selection.nextest_commands[0].mode,
            NextestCommandMode::Module
        );
        assert_eq!(
            selection.nextest_commands[0].args,
            vec!["-p", "api-lib", "http::tests"]
        );
    }

    #[test]
    fn mixed_src_test_module_and_production_edit_uses_package_dag_selection() {
        let mut metadata = package_metadata("api-lib/src/lib.rs");
        push_test_module(&mut metadata, "tests", 10, 20);
        let diff = diff_current_index(&metadata, &["api-lib/src/lib.rs".to_string()]);
        let selection = select_test_crates(
            &metadata,
            &ferris_report(),
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[ChangedFileInput {
                path: "api-lib/src/lib.rs".to_string(),
                ranges: vec![
                    ChangedLineRange {
                        start_line: 5,
                        end_line: 5,
                    },
                    ChangedLineRange {
                        start_line: 12,
                        end_line: 12,
                    },
                ],
            }],
        );

        assert_eq!(selection.reason, REASON_TEST_FERRIS_DAG);
        assert_eq!(selection.affected_test_crates.len(), 2);
        assert!(
            selection
                .nextest_commands
                .iter()
                .all(|command| command.mode == NextestCommandMode::Package)
        );
    }

    #[test]
    fn unknown_line_ranges_do_not_select_module_filter() {
        let mut metadata = package_metadata("api-lib/src/lib.rs");
        push_test_module(&mut metadata, "tests", 10, 20);
        let diff = diff_current_index(&metadata, &["api-lib/src/lib.rs".to_string()]);
        let selection = select_test_crates(
            &metadata,
            &ferris_report(),
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[ChangedFileInput {
                path: "api-lib/src/lib.rs".to_string(),
                ranges: Vec::new(),
            }],
        );

        assert_eq!(selection.reason, REASON_TEST_FERRIS_DAG);
        assert!(
            selection
                .nextest_commands
                .iter()
                .all(|command| command.mode == NextestCommandMode::Package)
        );
    }

    #[test]
    fn changed_lines_in_integration_test_file_helper_select_test_target() {
        let metadata = package_metadata("api-lib/tests/the_tests/http_auth.rs");
        let diff = diff_current_index(
            &metadata,
            &["api-lib/tests/the_tests/http_auth.rs".to_string()],
        );
        let selection = select_test_crates(
            &metadata,
            &ferris_report_for_api_lib_test(),
            &diff,
            &BTreeMap::<String, Vec<String>>::new(),
            &[ChangedFileInput {
                path: "api-lib/tests/the_tests/http_auth.rs".to_string(),
                ranges: vec![ChangedLineRange {
                    start_line: 2,
                    end_line: 2,
                }],
            }],
        );

        assert_eq!(selection.nextest_commands.len(), 1);
        assert_eq!(
            selection.nextest_commands[0].mode,
            NextestCommandMode::TestTarget
        );
        assert_eq!(
            selection.nextest_commands[0].args,
            vec!["-p", "api-lib", "--test", "http_auth"]
        );
    }

    #[test]
    fn module_command_validation_widens_to_package_when_no_tests_match() {
        let mut selection = TestSelection {
            affected_test_crates: Vec::new(),
            directly_affected_test_crates: Vec::new(),
            selected_test_packages: Vec::new(),
            nextest_commands: vec![NextestCommand {
                workspace: "nodes".to_string(),
                args: vec![
                    "-p".to_string(),
                    "api-lib".to_string(),
                    "missing".to_string(),
                ],
                mode: NextestCommandMode::Module,
                module_filter: Some("missing".to_string()),
                widened_from: None,
                matched_tests: Vec::new(),
                reason: "module".to_string(),
                changed_files: vec!["api-lib/tests/http.rs".to_string()],
            }],
            test_selection_checks: Vec::new(),
            workspace_selections: Vec::new(),
            reason: "module".to_string(),
            failed_wide: false,
        };

        validate_nextest_commands(
            &AffectedOptions {
                root: PathBuf::from("."),
                files: Vec::new(),
                changed_files: Vec::new(),
                base_cache: None,
                current_cache: None,
            },
            &[AffectedWorkspace {
                name: "nodes".to_string(),
                path: "nodes".to_string(),
            }],
            &mut selection,
            &EmptyNextestRunner,
        );

        assert_eq!(
            selection.nextest_commands[0].mode,
            NextestCommandMode::Package
        );
        assert_eq!(
            selection.nextest_commands[0].widened_from,
            Some(NextestCommandMode::Module)
        );
        assert_eq!(selection.nextest_commands[0].args, vec!["-p", "api-lib"]);
    }

    #[test]
    fn workspace_insertion_deduplicates_absolute_and_relative_paths_by_name() {
        let mut ferris = FerrisAffectedReport {
            affected_crates: Vec::new(),
            directly_affected_crates: Vec::new(),
            affected_workspaces: vec![AffectedWorkspace {
                name: "cargo-litmus".to_string(),
                path: "/repo/sandbox/cargo-litmus".to_string(),
            }],
            directly_affected_workspaces: Vec::new(),
        };

        insert_affected_workspace(
            &mut ferris,
            AffectedWorkspace {
                name: "cargo-litmus".to_string(),
                path: "sandbox/cargo-litmus".to_string(),
            },
        );

        assert_eq!(ferris.affected_workspaces.len(), 1);
        assert_eq!(ferris.directly_affected_workspaces.len(), 1);
    }

    #[test]
    fn manifest_changes_report_semantic_sections_distinctly() {
        let contents = "[features]\ndefault = []\n[target.'cfg(unix)'.dependencies]\nlibc = \
                        \"1\"\n[profile.release]\nlto = true\n[workspace.dependencies]\nserde = \
                        \"1\"\n";
        let ranges = [2, 4, 6, 8]
            .into_iter()
            .map(|line| ChangedLineRange {
                start_line: line,
                end_line: line,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            manifest_change_kinds(Some(contents), &ranges),
            vec![
                CargoChangeKind::Features,
                CargoChangeKind::TargetSpecificDependencies,
                CargoChangeKind::Profiles,
                CargoChangeKind::WorkspaceInheritance,
            ]
        );
    }
}
