use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use cargo_metadata::{Metadata, MetadataCommand, PackageId};
use git2::Repository;

use crate::error::{LitmusError, Result};
use crate::model::{IndexMetadata, PackageDependency, PackageNode, TargetNode};

#[derive(Clone, Debug, Default)]
pub struct CargoGraph {
    pub packages: Vec<PackageNode>,
    pub targets: Vec<TargetNode>,
    pub package_dependencies: Vec<PackageDependency>,
    pub metadata_hash: String,
}

pub fn build_cargo_graph(root: &Path) -> Result<CargoGraph> {
    let manifests = candidate_manifests(root);
    let mut workspace_roots = BTreeSet::new();
    let mut metadata_reports = Vec::new();

    for manifest in manifests {
        let metadata = MetadataCommand::new()
            .manifest_path(root.join(&manifest))
            .current_dir(root)
            .exec()
            .map_err(|source| LitmusError::CargoMetadata {
                manifest: root.join(&manifest),
                source,
            })?;

        let workspace_root = normalize_metadata_path(root, metadata.workspace_root.as_std_path());
        if workspace_roots.insert(workspace_root) {
            metadata_reports.push((manifest, metadata));
        }
    }

    Ok(cargo_graph_from_metadata(root, metadata_reports))
}

pub fn cached_cargo_graph(metadata: &IndexMetadata) -> CargoGraph {
    CargoGraph {
        packages: metadata.packages.clone(),
        targets: metadata.targets.clone(),
        package_dependencies: metadata.package_dependencies.clone(),
        metadata_hash: metadata.cargo_metadata_hash.clone(),
    }
}

pub fn cargo_graph_input_hash(root: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    for path in tracked_cargo_graph_inputs(root) {
        hasher.update(path.as_bytes());
        let full_path = root.join(&path);
        let bytes = fs::read(&full_path).map_err(|source| LitmusError::io(&full_path, source))?;
        hasher.update(blake3::hash(&bytes).as_bytes());
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn cargo_graph_from_metadata(root: &Path, metadata_reports: Vec<(String, Metadata)>) -> CargoGraph {
    let workspace_names_by_root = metadata_reports
        .iter()
        .filter(|(manifest, _)| manifest_contains(root, manifest, "[workspace]"))
        .map(|(_, metadata)| {
            let workspace_root =
                normalize_metadata_path(root, metadata.workspace_root.as_std_path());
            let workspace_name = workspace_name(&workspace_root);
            (workspace_root, workspace_name)
        })
        .collect::<BTreeMap<_, _>>();
    let mut packages_by_id = BTreeMap::new();
    let mut targets_by_id = BTreeMap::new();
    let mut dependencies = BTreeSet::new();
    let mut local_package_ids_by_metadata_id = BTreeMap::<PackageId, String>::new();

    for (_, metadata) in &metadata_reports {
        for package in metadata
            .packages
            .iter()
            .filter(|package| package.source.is_none())
            .filter(|package| path_is_repo_relative(root, package.manifest_path.as_std_path()))
        {
            let manifest_path = normalize_metadata_path(root, package.manifest_path.as_std_path());
            let root_path = parent_path(&manifest_path);
            let workspace_root =
                workspace_root_for_package_root(&root_path, &workspace_names_by_root)
                    .unwrap_or_else(|| root_path.clone());
            let workspace = workspace_names_by_root
                .get(&workspace_root)
                .cloned()
                .unwrap_or_else(|| package.name.to_string());
            let id = package_id(&manifest_path);
            local_package_ids_by_metadata_id.insert(package.id.clone(), id.clone());
            packages_by_id.insert(
                id.clone(),
                PackageNode {
                    id: id.clone(),
                    name: package.name.to_string(),
                    workspace,
                    workspace_path: workspace_root,
                    manifest_path,
                    root_path,
                },
            );

            for target in &package.targets {
                let src_path = normalize_metadata_path(root, target.src_path.as_std_path());
                let target_id = target_id(&id, &target.name, &src_path);
                targets_by_id.insert(
                    target_id.clone(),
                    TargetNode {
                        id: target_id,
                        package_id: id.clone(),
                        name: target.name.to_string(),
                        kind: target.kind.iter().map(ToString::to_string).collect(),
                        test: target.test,
                        src_path,
                    },
                );
            }
        }
    }

    for (_, metadata) in &metadata_reports {
        let Some(resolve) = &metadata.resolve else {
            continue;
        };
        for node in &resolve.nodes {
            let Some(package_id) = local_package_ids_by_metadata_id.get(&node.id) else {
                continue;
            };
            for dependency in &node.deps {
                let Some(depends_on_package_id) =
                    local_package_ids_by_metadata_id.get(&dependency.pkg)
                else {
                    continue;
                };
                let depends_on_path = packages_by_id
                    .get(depends_on_package_id)
                    .map(|package| package.root_path.clone())
                    .unwrap_or_default();
                if dependency.dep_kinds.is_empty() {
                    dependencies.insert((
                        package_id.clone(),
                        depends_on_package_id.clone(),
                        dependency.name.clone(),
                        depends_on_path.clone(),
                        "normal".to_string(),
                        None,
                    ));
                    continue;
                }
                for kind in &dependency.dep_kinds {
                    dependencies.insert((
                        package_id.clone(),
                        depends_on_package_id.clone(),
                        dependency.name.clone(),
                        depends_on_path.clone(),
                        kind.kind.to_string(),
                        kind.target.as_ref().map(ToString::to_string),
                    ));
                }
            }
        }
    }

    let packages = packages_by_id.into_values().collect::<Vec<_>>();
    let targets = targets_by_id.into_values().collect::<Vec<_>>();
    let package_dependencies = dependencies
        .into_iter()
        .map(
            |(package_id, depends_on_package_id, name, depends_on_path, kind, target)| {
                PackageDependency {
                    package_id,
                    depends_on_package_id,
                    name,
                    depends_on_path,
                    kind,
                    target,
                }
            },
        )
        .collect::<Vec<_>>();

    CargoGraph {
        metadata_hash: graph_hash(&packages, &targets, &package_dependencies),
        packages,
        targets,
        package_dependencies,
    }
}

fn path_is_repo_relative(root: &Path, path: &Path) -> bool {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    absolute.starts_with(root)
}

fn graph_hash(
    packages: &[PackageNode],
    targets: &[TargetNode],
    package_dependencies: &[PackageDependency],
) -> String {
    let mut hasher = blake3::Hasher::new();
    for package in packages {
        hasher.update(package.id.as_bytes());
        hasher.update(package.name.as_bytes());
        hasher.update(package.workspace.as_bytes());
        hasher.update(package.workspace_path.as_bytes());
        hasher.update(package.manifest_path.as_bytes());
        hasher.update(package.root_path.as_bytes());
    }
    for target in targets {
        hasher.update(target.id.as_bytes());
        hasher.update(target.package_id.as_bytes());
        hasher.update(target.name.as_bytes());
        hasher.update(target.src_path.as_bytes());
        hasher.update(if target.test { b"test" } else { b"not-test" });
        for kind in &target.kind {
            hasher.update(kind.as_bytes());
        }
    }
    for dependency in package_dependencies {
        hasher.update(dependency.package_id.as_bytes());
        hasher.update(dependency.depends_on_package_id.as_bytes());
        hasher.update(dependency.name.as_bytes());
        hasher.update(dependency.depends_on_path.as_bytes());
        hasher.update(dependency.kind.as_bytes());
        if let Some(target) = &dependency.target {
            hasher.update(target.as_bytes());
        }
    }
    hasher.finalize().to_hex().to_string()
}

fn candidate_manifests(root: &Path) -> Vec<String> {
    let cargo_tomls = tracked_cargo_manifests(root);
    let workspace_manifests = cargo_tomls
        .iter()
        .filter(|manifest| manifest_contains(root, manifest, "[workspace]"))
        .cloned()
        .collect::<BTreeSet<_>>();
    let workspace_roots = workspace_manifests
        .iter()
        .map(|manifest| parent_path(manifest))
        .collect::<Vec<_>>();

    cargo_tomls
        .into_iter()
        .filter(|manifest| {
            workspace_manifests.contains(manifest)
                || !workspace_roots
                    .iter()
                    .any(|workspace_root| path_is_under(&parent_path(manifest), workspace_root))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn tracked_cargo_manifests(root: &Path) -> Vec<String> {
    tracked_index_paths(root)
        .into_iter()
        .filter(|path| path.ends_with("Cargo.toml"))
        .collect()
}

fn tracked_cargo_graph_inputs(root: &Path) -> Vec<String> {
    tracked_index_paths(root)
        .into_iter()
        .filter(|path| {
            path.ends_with("Cargo.toml")
                || path.ends_with("Cargo.lock")
                || path.ends_with("build.rs")
                || path == ".cargo/config.toml"
                || path == ".cargo/config"
                || path.ends_with("/.cargo/config.toml")
                || path.ends_with("/.cargo/config")
        })
        .collect()
}

fn tracked_index_paths(root: &Path) -> Vec<String> {
    let Ok(repo) = Repository::discover(root) else {
        return Vec::new();
    };
    let Ok(index) = repo.index() else {
        return Vec::new();
    };
    index
        .iter()
        .filter_map(|entry| std::str::from_utf8(&entry.path).ok().map(str::to_string))
        .collect()
}

fn manifest_contains(root: &Path, manifest: &str, needle: &str) -> bool {
    fs::read_to_string(root.join(manifest)).is_ok_and(|contents| contents.contains(needle))
}

fn normalize_metadata_path(root: &Path, path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let relative = absolute.strip_prefix(&root).unwrap_or(&absolute);
    normalize_path(&relative.to_string_lossy())
}

fn parent_path(path: &str) -> String {
    Path::new(path)
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_string_lossy()
        .replace('\\', "/")
}

fn path_is_under(path: &str, root: &str) -> bool {
    root.is_empty() || path == root || path.starts_with(&format!("{root}/"))
}

fn workspace_root_for_package_root(
    package_root: &str,
    workspace_names_by_root: &BTreeMap<String, String>,
) -> Option<String> {
    workspace_names_by_root
        .iter()
        .filter(|(workspace_root, _)| path_is_under(package_root, workspace_root))
        .max_by_key(|(workspace_root, _)| workspace_root.len())
        .map(|(workspace_root, _)| workspace_root.clone())
}

fn workspace_name(workspace_root: &str) -> String {
    Path::new(workspace_root)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("root")
        .to_string()
}

fn normalize_path(path: &str) -> String {
    path.trim()
        .trim_start_matches("./")
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string()
}

fn package_id(manifest_path: &str) -> String {
    blake3::hash(format!("package\0{manifest_path}").as_bytes())
        .to_hex()
        .to_string()
}

fn target_id(package_id: &str, name: &str, src_path: &str) -> String {
    blake3::hash(format!("target\0{package_id}\0{name}\0{src_path}").as_bytes())
        .to_hex()
        .to_string()
}
