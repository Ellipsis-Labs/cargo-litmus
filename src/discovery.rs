use std::path::{Path, PathBuf};

use git2::{Index, Repository};

use crate::error::{LitmusError, Result};

#[derive(Clone, Debug)]
pub struct SourceFile {
    pub repo_relative_path: String,
    pub absolute_path: PathBuf,
}

#[derive(Debug)]
pub struct DiscoveryResult {
    pub repo_root: PathBuf,
    pub scan_root: PathBuf,
    pub files: Vec<SourceFile>,
    pub inaccessible_files: Vec<String>,
}

pub fn discover_rust_files(root: &Path) -> Result<DiscoveryResult> {
    let repo = Repository::discover(root).map_err(|_| LitmusError::RepoNotFound {
        path: root.to_path_buf(),
    })?;
    let repo_root = repo.workdir().ok_or(LitmusError::BareRepo)?.to_path_buf();
    let scan_root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| LitmusError::io(".", source))?
            .join(root)
    }
    .canonicalize()
    .map_err(|source| LitmusError::io(root, source))?;

    let index = repo.index()?;
    let mut files = Vec::new();
    let mut inaccessible_files = Vec::new();
    collect_index_paths(
        &index,
        &repo_root,
        &scan_root,
        &mut files,
        &mut inaccessible_files,
    )?;

    files.sort_by(|a, b| a.repo_relative_path.cmp(&b.repo_relative_path));
    inaccessible_files.sort();

    Ok(DiscoveryResult {
        repo_root,
        scan_root,
        files,
        inaccessible_files,
    })
}

fn collect_index_paths(
    index: &Index,
    repo_root: &Path,
    scan_root: &Path,
    files: &mut Vec<SourceFile>,
    inaccessible_files: &mut Vec<String>,
) -> Result<()> {
    for entry in index.iter() {
        if entry.mode == 0o160000 {
            continue;
        }

        let path_str =
            std::str::from_utf8(&entry.path).map_err(|_| LitmusError::InvalidUtf8Path {
                path: PathBuf::from(String::from_utf8_lossy(&entry.path).to_string()),
            })?;

        if !path_str.ends_with(".rs") || should_skip_repo_path(path_str) {
            continue;
        }

        let absolute_path = repo_root.join(path_str);
        if !absolute_path.starts_with(scan_root) {
            continue;
        }

        match std::fs::symlink_metadata(&absolute_path) {
            Ok(metadata) => {
                if metadata.is_symlink() || !metadata.is_file() {
                    continue;
                }
            }
            Err(_) => {
                inaccessible_files.push(path_str.to_string());
                continue;
            }
        }

        files.push(SourceFile {
            repo_relative_path: path_str.to_string(),
            absolute_path,
        });
    }

    Ok(())
}

fn should_skip_repo_path(path: &str) -> bool {
    path.split('/')
        .any(|component| matches!(component, ".git" | "target" | "node_modules"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_generated_dirs() {
        assert!(should_skip_repo_path("crate/target/debug/build.rs"));
        assert!(should_skip_repo_path("node_modules/pkg/src/lib.rs"));
        assert!(!should_skip_repo_path("src/lib.rs"));
    }
}
