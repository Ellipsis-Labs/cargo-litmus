use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::cargo_graph::{build_cargo_graph, cached_cargo_graph, cargo_graph_input_hash};
use crate::changed_files::ChangedFileInput;
use crate::discovery::discover_rust_files;
use crate::error::{LitmusError, Result};
use crate::model::{
    Edge, EdgeKind, FileNode, INDEX_VERSION, IndexMetadata, ModuleDeclarationNode,
    SourceDependencyNode,
};
use crate::parser::parse_rust_file;
use crate::storage::{load_index, save_index};

#[derive(Clone, Debug)]
pub struct IndexOptions {
    pub root: PathBuf,
    pub cache_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct IndexSummary {
    pub version: u32,
    pub tool_version: String,
    pub repo_root_hint: String,
    pub git_head: Option<String>,
    pub rust_file_list_hash: String,
    pub tracked_file_list_hash: String,
    pub cargo_metadata_hash: String,
    pub cache_path: String,
    pub files_parsed: usize,
    pub items_indexed: usize,
    pub packages_indexed: usize,
    pub targets_indexed: usize,
    pub package_dependencies: usize,
    pub edges: usize,
    pub parse_failures: usize,
    pub source_bytes: u64,
    pub cache_bytes: u64,
    pub elapsed_ms: u128,
    pub index_mode: IndexMode,
    pub files_reused: usize,
    pub files_reparsed: usize,
    pub files_removed: usize,
    pub files_added: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexMode {
    ColdRebuild,
    IncrementalUpdate,
    WarmUnmodified,
}

#[derive(Clone, Debug, Serialize)]
pub struct IndexReport {
    pub summary: IndexSummary,
    pub metadata: Option<IndexMetadata>,
}

pub fn build_index(options: &IndexOptions) -> Result<(IndexMetadata, IndexSummary)> {
    let started_at = Instant::now();
    let discovery = discover_rust_files(&options.root)?;
    let cache_path = options
        .cache_path
        .clone()
        .unwrap_or_else(|| defaulted_cache_path(&options.root));

    if !discovery.inaccessible_files.is_empty() {
        return Err(index_failures(discovery.inaccessible_files));
    }

    let mut parsed_files = Vec::new();
    let mut source_bytes = 0_u64;

    for source_file in &discovery.files {
        let parsed = parse_source_file(source_file)?;
        source_bytes += parsed.file.byte_len;
        parsed_files.push(parsed);
    }

    let mut cargo_graph = build_cargo_graph(&discovery.repo_root)?;
    cargo_graph.metadata_hash = cargo_graph_input_hash(&discovery.repo_root)?;
    let mut metadata = metadata_from_parts(&discovery.repo_root, parsed_files, cargo_graph)?;
    rebuild_module_edges(&mut metadata);
    let metadata = metadata.sorted();
    let cache_bytes = save_index(&cache_path, &metadata)?;

    let summary = summary_for_index(IndexSummaryInput {
        metadata: &metadata,
        cache_path: &cache_path,
        source_bytes,
        cache_bytes,
        elapsed_ms: started_at.elapsed().as_millis(),
        index_mode: IndexMode::ColdRebuild,
        files_reused: 0,
        files_reparsed: metadata.files.len(),
        files_removed: 0,
        files_added: metadata.files.len(),
    });

    Ok((metadata, summary))
}

pub fn build_or_update_index(
    options: &IndexOptions,
    changed_files: &[ChangedFileInput],
) -> Result<(IndexMetadata, IndexSummary)> {
    let started_at = Instant::now();
    let discovery = discover_rust_files(&options.root)?;
    let cache_path = options
        .cache_path
        .clone()
        .unwrap_or_else(|| defaulted_cache_path(&options.root));
    let changed_paths = changed_files
        .iter()
        .map(|file| file.path.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let unexplained_inaccessible = discovery
        .inaccessible_files
        .iter()
        .filter(|path| !changed_paths.contains(*path) || discovery.repo_root.join(path).exists())
        .cloned()
        .collect::<Vec<_>>();
    if !unexplained_inaccessible.is_empty() {
        return Err(index_failures(unexplained_inaccessible));
    }
    let repo_root_hint = discovery.repo_root.display().to_string();
    let Some(previous) = load_reusable_index(&cache_path) else {
        return build_index(options);
    };
    let portable_cache = previous.repo_root_hint != repo_root_hint;
    let current_tracked_file_list_hash = tracked_repository_file_list_hash(&discovery.repo_root)?;
    if portable_cache && previous.tracked_file_list_hash != current_tracked_file_list_hash {
        return build_index(options);
    }
    let current_cargo_graph_hash =
        if portable_cache || changed_paths.iter().any(|path| is_cargo_graph_input(path)) {
            let cargo_graph_hash = cargo_graph_input_hash(&discovery.repo_root)?;
            if previous.cargo_metadata_hash != cargo_graph_hash {
                return build_index(options);
            }
            Some(cargo_graph_hash)
        } else {
            None
        };
    let previous_lookup = PreviousIndexLookup::new(&previous);
    let discovered_paths = discovery
        .files
        .iter()
        .map(|file| file.repo_relative_path.clone())
        .collect::<Vec<_>>();
    let discovered_rust_file_list_hash =
        tracked_file_list_hash_for_paths(discovered_paths.iter().map(String::as_str));
    let unchanged_file_list = previous.rust_file_list_hash == discovered_rust_file_list_hash;
    let discovered_by_path: HashMap<_, _> = discovery
        .files
        .iter()
        .map(|file| (file.repo_relative_path.clone(), file))
        .collect();
    let previous_by_path: HashMap<_, _> = previous
        .files
        .iter()
        .map(|file| (file.path.clone(), file))
        .collect();

    let mut parsed_files = Vec::new();
    let mut files_reused = 0;
    let mut files_reparsed = 0;
    let mut files_added = 0;
    let mut files_removed = 0;

    for source_file in &discovery.files {
        let path = &source_file.repo_relative_path;
        let previous_file = previous_by_path.get(path).copied();
        if let Some(previous_file) = previous_file {
            if changed_paths.contains(path) {
                let current_hash = source_file_hash(source_file)?;
                if previous_file.hash == current_hash {
                    files_reused += 1;
                    parsed_files.push(ParsedSourceFile::from_previous_lookup(
                        &previous_lookup,
                        path,
                    ));
                } else {
                    let parsed = parse_source_file(source_file)?;
                    files_reparsed += 1;
                    parsed_files.push(parsed);
                }
            } else if unchanged_file_list && !portable_cache {
                files_reused += 1;
                parsed_files.push(ParsedSourceFile::from_previous_lookup(
                    &previous_lookup,
                    path,
                ));
            } else {
                let current_hash = source_file_hash(source_file)?;
                if previous_file.hash == current_hash {
                    files_reused += 1;
                    parsed_files.push(ParsedSourceFile::from_previous_lookup(
                        &previous_lookup,
                        path,
                    ));
                } else {
                    let parsed = parse_source_file(source_file)?;
                    files_reparsed += 1;
                    parsed_files.push(parsed);
                }
            }
        } else {
            let parsed = parse_source_file(source_file)?;
            files_added += 1;
            files_reparsed += 1;
            parsed_files.push(parsed);
        }
    }

    for previous_file in &previous.files {
        if !discovered_by_path.contains_key(&previous_file.path) {
            files_removed += 1;
        }
    }

    let identity_changed = portable_cache
        || previous.git_head != git_head(&discovery.repo_root)
        || previous.tracked_file_list_hash != current_tracked_file_list_hash
        || current_cargo_graph_hash
            .as_ref()
            .is_some_and(|hash| *hash != previous.cargo_metadata_hash);
    let index_mode =
        if files_reparsed == 0 && files_removed == 0 && files_added == 0 && !identity_changed {
            IndexMode::WarmUnmodified
        } else {
            IndexMode::IncrementalUpdate
        };
    if index_mode == IndexMode::WarmUnmodified {
        let cache_bytes = fs::metadata(&cache_path)
            .map(|metadata| metadata.len())
            .unwrap_or_default();
        let source_bytes = previous.files.iter().map(|file| file.byte_len).sum();
        let summary = summary_for_index(IndexSummaryInput {
            metadata: &previous,
            cache_path: &cache_path,
            source_bytes,
            cache_bytes,
            elapsed_ms: started_at.elapsed().as_millis(),
            index_mode,
            files_reused,
            files_reparsed,
            files_removed,
            files_added,
        });
        return Ok((previous, summary));
    }

    let mut metadata = metadata_from_parts(
        &discovery.repo_root,
        parsed_files,
        cached_cargo_graph(&previous),
    )?;
    rebuild_module_edges(&mut metadata);
    let metadata = metadata.sorted();

    let cache_bytes = save_index(&cache_path, &metadata)?;
    let source_bytes = metadata.files.iter().map(|file| file.byte_len).sum();

    let summary = summary_for_index(IndexSummaryInput {
        metadata: &metadata,
        cache_path: &cache_path,
        source_bytes,
        cache_bytes,
        elapsed_ms: started_at.elapsed().as_millis(),
        index_mode,
        files_reused,
        files_reparsed,
        files_removed,
        files_added,
    });

    Ok((metadata, summary))
}

#[derive(Clone, Debug)]
struct ParsedSourceFile {
    file: FileNode,
    items: Vec<crate::model::ItemNode>,
    contains_edges: Vec<Edge>,
    module_declarations: Vec<ModuleDeclarationNode>,
    source_dependencies: Vec<SourceDependencyNode>,
}

impl ParsedSourceFile {
    fn from_previous_lookup(previous: &PreviousIndexLookup, path: &str) -> Self {
        let file = previous
            .files_by_path
            .get(path)
            .expect("previous file exists")
            .clone();
        Self {
            items: previous
                .items_by_file_id
                .get(&file.id)
                .cloned()
                .unwrap_or_default(),
            contains_edges: previous
                .contains_edges_by_file_id
                .get(&file.id)
                .cloned()
                .unwrap_or_default(),
            module_declarations: previous
                .module_declarations_by_file_id
                .get(&file.id)
                .cloned()
                .unwrap_or_default(),
            source_dependencies: previous
                .source_dependencies_by_path
                .get(path)
                .cloned()
                .unwrap_or_default(),
            file,
        }
    }
}

struct PreviousIndexLookup {
    files_by_path: HashMap<String, FileNode>,
    items_by_file_id: HashMap<String, Vec<crate::model::ItemNode>>,
    contains_edges_by_file_id: HashMap<String, Vec<Edge>>,
    module_declarations_by_file_id: HashMap<String, Vec<ModuleDeclarationNode>>,
    source_dependencies_by_path: HashMap<String, Vec<SourceDependencyNode>>,
}

impl PreviousIndexLookup {
    fn new(previous: &IndexMetadata) -> Self {
        let files_by_path = previous
            .files
            .iter()
            .map(|file| (file.path.clone(), file.clone()))
            .collect::<HashMap<_, _>>();
        let file_ids = previous
            .files
            .iter()
            .map(|file| file.id.clone())
            .collect::<std::collections::BTreeSet<_>>();

        let mut items_by_file_id = HashMap::<String, Vec<crate::model::ItemNode>>::new();
        let mut file_id_by_item_id = HashMap::<String, String>::new();
        for item in &previous.items {
            file_id_by_item_id.insert(item.id.clone(), item.file_id.clone());
            items_by_file_id
                .entry(item.file_id.clone())
                .or_default()
                .push(item.clone());
        }

        let mut contains_edges_by_file_id = HashMap::<String, Vec<Edge>>::new();
        for edge in previous
            .edges
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Contains)
        {
            let file_id = if file_ids.contains(&edge.from) {
                Some(edge.from.clone())
            } else {
                file_id_by_item_id.get(&edge.from).cloned()
            };
            if let Some(file_id) = file_id {
                contains_edges_by_file_id
                    .entry(file_id)
                    .or_default()
                    .push(edge.clone());
            }
        }

        let mut module_declarations_by_file_id =
            HashMap::<String, Vec<ModuleDeclarationNode>>::new();
        for declaration in &previous.module_declarations {
            module_declarations_by_file_id
                .entry(declaration.file_id.clone())
                .or_default()
                .push(declaration.clone());
        }
        let mut source_dependencies_by_path = HashMap::<String, Vec<SourceDependencyNode>>::new();
        for dependency in &previous.source_dependencies {
            source_dependencies_by_path
                .entry(dependency.source_path.clone())
                .or_default()
                .push(dependency.clone());
        }

        Self {
            files_by_path,
            items_by_file_id,
            contains_edges_by_file_id,
            module_declarations_by_file_id,
            source_dependencies_by_path,
        }
    }
}

fn parse_source_file(source_file: &crate::discovery::SourceFile) -> Result<ParsedSourceFile> {
    let repo_path = &source_file.repo_relative_path;
    let bytes = fs::read(&source_file.absolute_path)
        .map_err(|source| LitmusError::io(&source_file.absolute_path, source))?;
    let source = std::str::from_utf8(&bytes).map_err(|_| LitmusError::InvalidUtf8Path {
        path: source_file.absolute_path.clone(),
    })?;

    let file = FileNode {
        id: file_id(repo_path),
        path: repo_path.clone(),
        hash: blake3::hash(&bytes).to_hex().to_string(),
        byte_len: bytes.len() as u64,
    };

    let parsed =
        parse_rust_file(repo_path, &file, source).map_err(|source| LitmusError::Parse {
            path: source_file.absolute_path.clone(),
            source,
        })?;
    let module_declarations = parsed
        .module_declarations
        .into_iter()
        .map(|declaration| ModuleDeclarationNode {
            file_id: file.id.clone(),
            from_item_id: declaration.from_item_id,
            target_paths: declaration.target_paths,
        })
        .collect();

    Ok(ParsedSourceFile {
        file,
        items: parsed.items,
        contains_edges: parsed.edges,
        module_declarations,
        source_dependencies: parsed.source_dependencies,
    })
}

fn source_file_hash(source_file: &crate::discovery::SourceFile) -> Result<String> {
    let bytes = fs::read(&source_file.absolute_path)
        .map_err(|source| LitmusError::io(&source_file.absolute_path, source))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn metadata_from_parts(
    repo_root: &Path,
    parsed_files: Vec<ParsedSourceFile>,
    cargo_graph: crate::cargo_graph::CargoGraph,
) -> Result<IndexMetadata> {
    let mut files = Vec::new();
    let mut items = Vec::new();
    let mut edges = Vec::new();
    let mut module_declarations = Vec::new();
    let mut source_dependencies = Vec::new();

    for parsed in parsed_files {
        files.push(parsed.file);
        items.extend(parsed.items);
        edges.extend(parsed.contains_edges);
        module_declarations.extend(parsed.module_declarations);
        source_dependencies.extend(parsed.source_dependencies);
    }

    Ok(IndexMetadata {
        version: INDEX_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        repo_root_hint: repo_root.display().to_string(),
        git_head: git_head(repo_root),
        rust_file_list_hash: tracked_file_list_hash(&files),
        tracked_file_list_hash: tracked_repository_file_list_hash(repo_root)?,
        cargo_metadata_hash: cargo_graph.metadata_hash,
        generated_at_unix_ms: now_ms(),
        files,
        items,
        packages: cargo_graph.packages,
        targets: cargo_graph.targets,
        package_dependencies: cargo_graph.package_dependencies,
        module_declarations,
        source_dependencies,
        edges,
    })
}

fn rebuild_module_edges(metadata: &mut IndexMetadata) {
    let file_ids_by_path: HashMap<_, _> = metadata
        .files
        .iter()
        .map(|file| (file.path.clone(), file.id.clone()))
        .collect();
    metadata
        .edges
        .retain(|edge| edge.kind != EdgeKind::ModuleDeclares);
    for declaration in &metadata.module_declarations {
        if let Some(target_file_id) = declaration
            .target_paths
            .iter()
            .find_map(|path| file_ids_by_path.get(path))
        {
            metadata.edges.push(Edge {
                from: declaration.from_item_id.clone(),
                to: target_file_id.clone(),
                kind: EdgeKind::ModuleDeclares,
            });
        }
    }
}

fn load_reusable_index(cache_path: &Path) -> Option<IndexMetadata> {
    let previous = load_index(cache_path).ok()?;
    if previous.version != INDEX_VERSION || previous.tool_version != env!("CARGO_PKG_VERSION") {
        return None;
    }
    Some(previous)
}

fn is_cargo_graph_input(path: &str) -> bool {
    path.ends_with("Cargo.toml")
        || path.ends_with("Cargo.lock")
        || path.ends_with("build.rs")
        || path == ".cargo/config.toml"
        || path == ".cargo/config"
        || path.ends_with("/.cargo/config.toml")
        || path.ends_with("/.cargo/config")
}

struct IndexSummaryInput<'a> {
    metadata: &'a IndexMetadata,
    cache_path: &'a Path,
    source_bytes: u64,
    cache_bytes: u64,
    elapsed_ms: u128,
    index_mode: IndexMode,
    files_reused: usize,
    files_reparsed: usize,
    files_removed: usize,
    files_added: usize,
}

fn summary_for_index(input: IndexSummaryInput<'_>) -> IndexSummary {
    let IndexSummaryInput {
        metadata,
        cache_path,
        source_bytes,
        cache_bytes,
        elapsed_ms,
        index_mode,
        files_reused,
        files_reparsed,
        files_removed,
        files_added,
    } = input;
    IndexSummary {
        version: metadata.version,
        tool_version: metadata.tool_version.clone(),
        repo_root_hint: metadata.repo_root_hint.clone(),
        git_head: metadata.git_head.clone(),
        rust_file_list_hash: metadata.rust_file_list_hash.clone(),
        tracked_file_list_hash: metadata.tracked_file_list_hash.clone(),
        cargo_metadata_hash: metadata.cargo_metadata_hash.clone(),
        cache_path: cache_path.display().to_string(),
        files_parsed: metadata.files.len(),
        items_indexed: metadata.items.len(),
        packages_indexed: metadata.packages.len(),
        targets_indexed: metadata.targets.len(),
        package_dependencies: metadata.package_dependencies.len(),
        edges: metadata.edges.len(),
        parse_failures: 0,
        source_bytes,
        cache_bytes,
        elapsed_ms,
        index_mode,
        files_reused,
        files_reparsed,
        files_removed,
        files_added,
    }
}

pub fn defaulted_cache_path(root: &Path) -> PathBuf {
    defaulted_cache_path_with_target_dir(root, std::env::var_os("CARGO_TARGET_DIR"))
}

fn defaulted_cache_path_with_target_dir(
    root: &Path,
    cargo_target_dir: Option<impl AsRef<OsStr>>,
) -> PathBuf {
    let target_dir = cargo_target_dir
        .map(|target_dir| PathBuf::from(target_dir.as_ref()))
        .filter(|target_dir| !target_dir.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("target"));
    let target_dir = if target_dir.is_absolute() {
        target_dir
    } else {
        root.join(target_dir)
    };

    target_dir.join("cargo-litmus.rkyv")
}

pub fn file_id(path: &str) -> String {
    blake3::hash(format!("file\0{path}").as_bytes())
        .to_hex()
        .to_string()
}

fn index_failures(paths: Vec<String>) -> LitmusError {
    LitmusError::IndexFailures {
        count: paths.len(),
        paths: paths.join(", "),
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn git_head(root: &Path) -> Option<String> {
    let repo = git2::Repository::discover(root).ok()?;
    let head = repo.head().ok()?;
    head.target().map(|oid| oid.to_string())
}

fn tracked_file_list_hash(files: &[FileNode]) -> String {
    tracked_file_list_hash_for_paths(files.iter().map(|file| file.path.as_str()))
}

fn tracked_file_list_hash_for_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> String {
    let mut hasher = blake3::Hasher::new();
    let mut paths = paths.into_iter().collect::<Vec<_>>();
    paths.sort_unstable();
    for path in paths {
        hasher.update(path.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn tracked_repository_file_list_hash(root: &Path) -> Result<String> {
    let repo = git2::Repository::discover(root)?;
    let index = repo.index()?;
    let mut paths = index
        .iter()
        .map(|entry| entry.path.to_vec())
        .collect::<Vec<_>>();
    paths.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    for path in paths {
        hasher.update(&path);
        hasher.update(&[0]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cache_path_lives_under_root_target() {
        assert_eq!(
            defaulted_cache_path_with_target_dir(Path::new("repo"), None::<&OsStr>),
            PathBuf::from("repo/target/cargo-litmus.rkyv")
        );
    }

    #[test]
    fn default_cache_path_respects_relative_cargo_target_dir() {
        assert_eq!(
            defaulted_cache_path_with_target_dir(
                Path::new("repo"),
                Some(OsStr::new(".cache/cargo-target"))
            ),
            PathBuf::from("repo/.cache/cargo-target/cargo-litmus.rkyv")
        );
    }

    #[test]
    fn default_cache_path_respects_absolute_cargo_target_dir() {
        assert_eq!(
            defaulted_cache_path_with_target_dir(
                Path::new("repo"),
                Some(OsStr::new("/tmp/cargo-target"))
            ),
            PathBuf::from("/tmp/cargo-target/cargo-litmus.rkyv")
        );
    }

    #[test]
    fn file_ids_are_deterministic() {
        assert_eq!(file_id("src/lib.rs"), file_id("src/lib.rs"));
        assert_ne!(file_id("src/lib.rs"), file_id("src/main.rs"));
    }
}
