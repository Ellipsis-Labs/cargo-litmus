use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use git2::{Delta, Diff, DiffFindOptions, DiffOptions, ObjectType, Oid, Repository, Tree};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{LitmusError, Result};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChangedFileInput {
    pub path: String,
    pub ranges: Vec<ChangedLineRange>,
}

impl ChangedFileInput {
    pub fn path_only(path: impl Into<String>) -> Self {
        Self {
            path: normalize_path(&path.into()),
            ranges: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ChangedLineRange {
    pub start_line: usize,
    pub end_line: usize,
}

/// A normalized set of repository changes and the Git state that produced it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChangeSet {
    pub source: ChangeSource,
    pub files: Vec<ChangedFileInput>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangeSource {
    Revisions {
        base: String,
        head: String,
    },
    MergeBase {
        upstream: String,
        base: String,
        head: String,
    },
    Worktree,
    Staged,
    Explicit,
    Combined {
        sources: Vec<ChangeSource>,
    },
}

impl ChangeSet {
    pub fn combine(change_sets: impl IntoIterator<Item = ChangeSet>) -> Self {
        let change_sets = change_sets.into_iter().collect::<Vec<_>>();
        Self {
            source: ChangeSource::Combined {
                sources: change_sets
                    .iter()
                    .map(|change_set| change_set.source.clone())
                    .collect(),
            },
            files: merge_changed_file_inputs(
                change_sets
                    .into_iter()
                    .flat_map(|change_set| change_set.files),
            ),
        }
    }

    pub fn explicit(files: impl IntoIterator<Item = ChangedFileInput>) -> Self {
        Self {
            source: ChangeSource::Explicit,
            files: merge_changed_file_inputs(files),
        }
    }

    pub fn revisions(root: &Path, base: &str, head: &str) -> Result<Self> {
        let repo = open_repository(root)?;
        let base_tree = revision_tree(&repo, base)?;
        let head_tree = revision_tree(&repo, head)?;
        let diff = repo.diff_tree_to_tree(Some(&base_tree), Some(&head_tree), None)?;
        Ok(Self {
            source: ChangeSource::Revisions {
                base: base.to_string(),
                head: head.to_string(),
            },
            files: changed_files_from_diff(diff)?,
        })
    }

    pub fn merge_base(root: &Path, upstream: &str) -> Result<Self> {
        let repo = open_repository(root)?;
        let upstream_oid = revision_oid(&repo, upstream)?;
        let head_oid = revision_oid(&repo, "HEAD")?;
        let base_oid = repo.merge_base(upstream_oid, head_oid)?;
        let base_tree = commit_tree(&repo, base_oid)?;
        let head_tree = commit_tree(&repo, head_oid)?;
        let diff = repo.diff_tree_to_tree(Some(&base_tree), Some(&head_tree), None)?;
        Ok(Self {
            source: ChangeSource::MergeBase {
                upstream: upstream.to_string(),
                base: base_oid.to_string(),
                head: head_oid.to_string(),
            },
            files: changed_files_from_diff(diff)?,
        })
    }

    pub fn staged(root: &Path) -> Result<Self> {
        let repo = open_repository(root)?;
        let head_tree = head_tree(&repo)?;
        let diff = repo.diff_tree_to_index(head_tree.as_ref(), None, None)?;
        Ok(Self {
            source: ChangeSource::Staged,
            files: changed_files_from_diff(diff)?,
        })
    }

    /// Return staged, unstaged, deleted, renamed, and untracked worktree
    /// changes.
    pub fn worktree(root: &Path) -> Result<Self> {
        let repo = open_repository(root)?;
        let mut files = Self::staged(root)?.files;
        let mut options = DiffOptions::new();
        options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_typechange(true);
        let diff = repo.diff_index_to_workdir(None, Some(&mut options))?;
        files.extend(changed_files_from_diff(diff)?);
        Ok(Self {
            source: ChangeSource::Worktree,
            files: merge_changed_file_inputs(files),
        })
    }
}

pub fn parse_changed_files(raw: &str) -> Vec<String> {
    parse_changed_file_inputs(raw)
        .into_iter()
        .map(|file| file.path)
        .collect()
}

pub fn parse_changed_file_inputs(raw: &str) -> Vec<ChangedFileInput> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        let mut files = Vec::new();
        collect_json_changed_files(&value, &mut files);
        return merge_changed_file_inputs(files);
    }

    merge_changed_file_inputs(
        trimmed
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ChangedFileInput::path_only),
    )
}

pub fn changed_file_inputs_from_revs(
    root: &Path,
    base_rev: &str,
    head_rev: &str,
) -> Result<Vec<ChangedFileInput>> {
    Ok(ChangeSet::revisions(root, base_rev, head_rev)?.files)
}

fn open_repository(root: &Path) -> Result<Repository> {
    Repository::discover(root).map_err(|_| LitmusError::RepoNotFound {
        path: root.to_path_buf(),
    })
}

fn revision_oid(repo: &Repository, revision: &str) -> Result<Oid> {
    let object = repo.revparse_single(revision)?;
    let commit = object.peel(ObjectType::Commit)?;
    Ok(commit.id())
}

fn revision_tree<'repo>(repo: &'repo Repository, revision: &str) -> Result<Tree<'repo>> {
    commit_tree(repo, revision_oid(repo, revision)?)
}

fn commit_tree(repo: &Repository, oid: Oid) -> Result<Tree<'_>> {
    Ok(repo.find_commit(oid)?.tree()?)
}

fn head_tree(repo: &Repository) -> Result<Option<Tree<'_>>> {
    match repo.head() {
        Ok(head) => Ok(Some(head.peel_to_tree()?)),
        Err(error) if error.code() == git2::ErrorCode::UnbornBranch => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn changed_files_from_diff(mut diff: Diff<'_>) -> Result<Vec<ChangedFileInput>> {
    let mut find = DiffFindOptions::new();
    find.renames(true).copies(true).rewrites(true);
    diff.find_similar(Some(&mut find))?;

    let mut by_path = BTreeMap::<String, Vec<ChangedLineRange>>::new();
    for delta in diff.deltas() {
        let old_path = delta.old_file().path().and_then(Path::to_str);
        let new_path = delta.new_file().path().and_then(Path::to_str);
        match delta.status() {
            Delta::Renamed | Delta::Copied => {
                insert_path_only(&mut by_path, old_path);
                insert_path_only(&mut by_path, new_path);
            }
            Delta::Deleted => insert_path_only(&mut by_path, old_path),
            _ => insert_path_only(&mut by_path, new_path.or(old_path)),
        }
    }

    diff.foreach(
        &mut |_delta, _progress| true,
        None,
        Some(&mut |delta, hunk| {
            let path = delta.new_file().path().or_else(|| delta.old_file().path());
            if let Some(path) = path.and_then(Path::to_str) {
                let start = usize::try_from(hunk.new_start()).unwrap_or(0);
                let lines = usize::try_from(hunk.new_lines()).unwrap_or(0);
                if start > 0 && lines > 0 {
                    by_path
                        .entry(normalize_path(path))
                        .or_default()
                        .push(ChangedLineRange {
                            start_line: start,
                            end_line: start + lines - 1,
                        });
                }
            }
            true
        }),
        None,
    )?;

    Ok(merge_changed_file_inputs(
        by_path
            .into_iter()
            .map(|(path, ranges)| ChangedFileInput { path, ranges }),
    ))
}

fn insert_path_only(by_path: &mut BTreeMap<String, Vec<ChangedLineRange>>, path: Option<&str>) {
    if let Some(path) = path {
        by_path.entry(normalize_path(path)).or_default();
    }
}

pub fn parse_unified_diff_ranges(raw: &str) -> Vec<ChangedFileInput> {
    let mut files = Vec::new();
    let mut current_path: Option<String> = None;
    let mut old_path: Option<String> = None;
    let mut current_ranges = Vec::new();

    for line in raw.lines() {
        if let Some(path) = line.strip_prefix("--- a/") {
            old_path = Some(normalize_path(path));
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ b/") {
            finish_current_diff_file(&mut files, &mut current_path, &mut current_ranges);
            current_path = Some(normalize_path(path));
            old_path = None;
            continue;
        }
        if line.starts_with("+++ /dev/null") {
            finish_current_diff_file(&mut files, &mut current_path, &mut current_ranges);
            current_path = old_path.take();
            continue;
        }
        let Some(header) = line.strip_prefix("@@ ") else {
            continue;
        };
        let Some(range) = header.split_whitespace().find(|part| part.starts_with('+')) else {
            continue;
        };
        if let Some(range) = parse_diff_range(range) {
            current_ranges.push(range);
        }
    }

    finish_current_diff_file(&mut files, &mut current_path, &mut current_ranges);

    merge_changed_file_inputs(files)
}

fn finish_current_diff_file(
    files: &mut Vec<ChangedFileInput>,
    current_path: &mut Option<String>,
    current_ranges: &mut Vec<ChangedLineRange>,
) {
    if let Some(path) = current_path.take() {
        files.push(ChangedFileInput {
            path,
            ranges: std::mem::take(current_ranges),
        });
    }
}

fn parse_diff_range(raw: &str) -> Option<ChangedLineRange> {
    let raw = raw.strip_prefix('+')?;
    let (start, len) = match raw.split_once(',') {
        Some((start, len)) => (start.parse::<usize>().ok()?, len.parse::<usize>().ok()?),
        None => (raw.parse::<usize>().ok()?, 1),
    };
    let end_line = if len == 0 { start } else { start + len - 1 };
    Some(ChangedLineRange {
        start_line: start,
        end_line,
    })
}

fn collect_json_changed_files(value: &Value, files: &mut Vec<ChangedFileInput>) {
    match value {
        Value::Array(entries) => {
            for entry in entries {
                collect_json_changed_files(entry, files);
            }
        }
        Value::Object(map) => {
            if let Some(path) = map.get("path").and_then(Value::as_str) {
                files.push(ChangedFileInput {
                    path: normalize_path(path),
                    ranges: parse_json_ranges(map.get("ranges")),
                });
                return;
            }
            for key in ["files", "paths", "changed_files", "items", "values"] {
                if let Some(value) = map.get(key) {
                    collect_json_changed_files(value, files);
                }
            }
        }
        Value::String(path) => files.push(ChangedFileInput::path_only(path.clone())),
        _ => {}
    }
}

fn parse_json_ranges(value: Option<&Value>) -> Vec<ChangedLineRange> {
    let Some(Value::Array(entries)) = value else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let start = entry
                .get("start_line")
                .or_else(|| entry.get("start"))
                .and_then(Value::as_u64)? as usize;
            let end = entry
                .get("end_line")
                .or_else(|| entry.get("end"))
                .and_then(Value::as_u64)
                .map(|line| line as usize)
                .unwrap_or(start);
            Some(ChangedLineRange {
                start_line: start,
                end_line: end,
            })
        })
        .collect()
}

pub fn merge_changed_file_inputs(
    files: impl IntoIterator<Item = ChangedFileInput>,
) -> Vec<ChangedFileInput> {
    let mut by_path = std::collections::BTreeMap::<String, Vec<ChangedLineRange>>::new();
    for file in files {
        let path = normalize_path(&file.path);
        if path.is_empty() {
            continue;
        }
        by_path.entry(path).or_default().extend(file.ranges);
    }

    by_path
        .into_iter()
        .map(|(path, ranges)| ChangedFileInput {
            path,
            ranges: normalize_ranges(ranges),
        })
        .collect()
}

fn normalize_ranges(ranges: Vec<ChangedLineRange>) -> Vec<ChangedLineRange> {
    ranges
        .into_iter()
        .filter(|range| range.start_line > 0 && range.end_line >= range.start_line)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn normalize_path(path: &str) -> String {
    path.trim()
        .trim_start_matches("./")
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use git2::{Repository, Signature};
    use tempfile::TempDir;

    use super::*;

    fn committed_repo(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        for (path, contents) in files {
            let full_path = dir.path().join(path);
            fs::create_dir_all(full_path.parent().unwrap()).unwrap();
            fs::write(full_path, contents).unwrap();
        }
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let signature = Signature::now("Litmus Test", "litmus@example.com").unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        drop(tree);
        drop(repo);
        dir
    }

    #[test]
    fn parses_newline_input() {
        assert_eq!(
            parse_changed_files("src/lib.rs\nsrc/main.rs\n"),
            vec!["src/lib.rs", "src/main.rs"]
        );
    }

    #[test]
    fn preserves_spaces_in_newline_delimited_paths() {
        assert_eq!(
            parse_changed_files("schemas/api schema.json\nsrc/main.rs\n"),
            vec!["schemas/api schema.json", "src/main.rs"]
        );
    }

    #[test]
    fn parses_json_array_input() {
        assert_eq!(
            parse_changed_files(r#"["src/lib.rs","src/main.rs"]"#),
            vec!["src/lib.rs", "src/main.rs"]
        );
    }

    #[test]
    fn parses_json_object_input() {
        assert_eq!(
            parse_changed_files(r#"{"changed_files":["src/lib.rs"]}"#),
            vec!["src/lib.rs"]
        );
    }

    #[test]
    fn parses_json_objects_with_ranges() {
        assert_eq!(
            parse_changed_file_inputs(
                r#"{"changed_files":[{"path":"src/lib.rs","ranges":[{"start_line":3,"end_line":5}]}]}"#
            ),
            vec![ChangedFileInput {
                path: "src/lib.rs".to_string(),
                ranges: vec![ChangedLineRange {
                    start_line: 3,
                    end_line: 5,
                }],
            }]
        );
    }

    #[test]
    fn parses_unified_diff_ranges() {
        let parsed = parse_unified_diff_ranges(
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,0 \
             +2,3 @@\n+one\n+two\n+three\n",
        );

        assert_eq!(
            parsed,
            vec![ChangedFileInput {
                path: "src/lib.rs".to_string(),
                ranges: vec![ChangedLineRange {
                    start_line: 2,
                    end_line: 4,
                }],
            }]
        );
    }

    #[test]
    fn parses_deleted_file_diff_path() {
        let parsed = parse_unified_diff_ranges(
            "diff --git a/src/old.rs b/src/old.rs\n--- a/src/old.rs\n+++ /dev/null\n@@ -1,2 +0,0 \
             @@\n-old\n-file\n",
        );

        assert_eq!(
            parsed,
            vec![ChangedFileInput {
                path: "src/old.rs".to_string(),
                ranges: Vec::new(),
            }]
        );
    }

    #[test]
    fn worktree_combines_staged_unstaged_and_untracked_changes() {
        let repo = committed_repo(&[
            ("src/staged.rs", "pub fn staged() {}\n"),
            ("src/unstaged.rs", "pub fn unstaged() {}\n"),
        ]);
        fs::write(
            repo.path().join("src/staged.rs"),
            "pub fn staged() { todo!() }\n",
        )
        .unwrap();
        let git = Repository::open(repo.path()).unwrap();
        let mut index = git.index().unwrap();
        index.add_path(Path::new("src/staged.rs")).unwrap();
        index.write().unwrap();
        fs::write(
            repo.path().join("src/unstaged.rs"),
            "pub fn unstaged() { todo!() }\n",
        )
        .unwrap();
        fs::write(repo.path().join("src/untracked.rs"), "pub fn new() {}\n").unwrap();

        let changes = ChangeSet::worktree(repo.path()).unwrap();
        let paths = changes
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec!["src/staged.rs", "src/unstaged.rs", "src/untracked.rs"]
        );
    }

    #[test]
    fn staged_rename_includes_old_and_new_paths() {
        let repo = committed_repo(&[("src/old.rs", "pub fn value() -> u64 { 1 }\n")]);
        fs::rename(
            repo.path().join("src/old.rs"),
            repo.path().join("src/new.rs"),
        )
        .unwrap();
        let git = Repository::open(repo.path()).unwrap();
        let mut index = git.index().unwrap();
        index.remove_path(Path::new("src/old.rs")).unwrap();
        index.add_path(Path::new("src/new.rs")).unwrap();
        index.write().unwrap();

        let changes = ChangeSet::staged(repo.path()).unwrap();
        let paths = changes
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["src/new.rs", "src/old.rs"]);
    }

    #[test]
    fn merge_base_compares_branch_head_to_common_ancestor() {
        let repo = committed_repo(&[("src/lib.rs", "pub fn value() -> u64 { 1 }\n")]);
        let git = Repository::open(repo.path()).unwrap();
        let base = git.head().unwrap().target().unwrap();
        fs::write(
            repo.path().join("src/lib.rs"),
            "pub fn value() -> u64 { 2 }\n",
        )
        .unwrap();
        let mut index = git.index().unwrap();
        index.add_path(Path::new("src/lib.rs")).unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = git.find_tree(tree_id).unwrap();
        let parent = git.find_commit(base).unwrap();
        let signature = Signature::now("Litmus Test", "litmus@example.com").unwrap();
        git.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "change",
            &tree,
            &[&parent],
        )
        .unwrap();

        let changes = ChangeSet::merge_base(repo.path(), &base.to_string()).unwrap();
        assert_eq!(changes.files.len(), 1);
        assert_eq!(changes.files[0].path, "src/lib.rs");
        assert!(!changes.files[0].ranges.is_empty());
    }

    #[test]
    fn combines_committed_staged_and_worktree_change_sets() {
        let combined = ChangeSet::combine([
            ChangeSet {
                source: ChangeSource::Revisions {
                    base: "base".to_string(),
                    head: "head".to_string(),
                },
                files: vec![ChangedFileInput::path_only("src/committed.rs")],
            },
            ChangeSet {
                source: ChangeSource::Staged,
                files: vec![ChangedFileInput::path_only("src/staged.rs")],
            },
            ChangeSet {
                source: ChangeSource::Worktree,
                files: vec![ChangedFileInput::path_only("src/untracked.rs")],
            },
        ]);

        assert!(matches!(combined.source, ChangeSource::Combined { .. }));
        assert_eq!(combined.files.len(), 3);
    }
}
