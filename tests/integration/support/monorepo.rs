//! Synthesis DSL for arbitrary multi-workspace Rust monorepos.
//!
//! [`Monorepo`] materializes a [`Workspace`]/[`Package`] spec into a temporary
//! git repository, commits it, and exposes the repository plumbing scenarios
//! need to author changes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use git2::{IndexAddOption, Repository, Signature, Time};
use tempfile::TempDir;

use super::env::sanitized_path_without_shim;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DepKind {
    Normal,
    Dev,
    Build,
}

impl DepKind {
    fn manifest_table(self) -> &'static str {
        match self {
            DepKind::Normal => "dependencies",
            DepKind::Dev => "dev-dependencies",
            DepKind::Build => "build-dependencies",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Dep {
    pub on: String,
    pub kind: DepKind,
    pub optional: bool,
    pub features: Vec<String>,
    pub path: Option<String>,
}

impl Dep {
    fn new(on: &str, kind: DepKind) -> Self {
        Self {
            on: on.to_string(),
            kind,
            optional: false,
            features: Vec::new(),
            path: None,
        }
    }

    fn manifest_entry(&self) -> String {
        let path = self
            .path
            .clone()
            .unwrap_or_else(|| format!("../{}", self.on));
        let mut entry = format!("{} = {{ path = \"{path}\"", self.on);
        if self.optional {
            entry.push_str(", optional = true");
        }
        if !self.features.is_empty() {
            entry.push_str(", features = [");
            for (index, feature) in self.features.iter().enumerate() {
                if index > 0 {
                    entry.push_str(", ");
                }
                entry.push_str(&format!("\"{feature}\""));
            }
            entry.push(']');
        }
        entry.push_str(" }");
        entry
    }
}

#[derive(Clone, Debug)]
pub struct Package {
    pub name: String,
    pub deps: Vec<Dep>,
    pub features: Vec<(String, Vec<String>)>,
    pub files: BTreeMap<String, String>,
    pub manifest_extra: Vec<String>,
    pub lib: Option<String>,
}

pub fn pkg(name: &str) -> Package {
    Package {
        name: name.to_string(),
        deps: Vec::new(),
        features: Vec::new(),
        files: BTreeMap::new(),
        manifest_extra: Vec::new(),
        lib: None,
    }
}

impl Package {
    pub fn dep(mut self, on: &str) -> Self {
        self.deps.push(Dep::new(on, DepKind::Normal));
        self
    }

    /// Dependency on a sibling package at an explicit relative path.
    pub fn dep_at(mut self, on: &str, path: &str) -> Self {
        self.deps.push(Dep {
            path: Some(path.to_string()),
            ..Dep::new(on, DepKind::Normal)
        });
        self
    }

    pub fn dep_with_features(mut self, on: &str, features: &[&str]) -> Self {
        self.deps.push(Dep {
            features: features.iter().map(|f| f.to_string()).collect(),
            ..Dep::new(on, DepKind::Normal)
        });
        self
    }

    pub fn dev_dep(mut self, on: &str) -> Self {
        self.deps.push(Dep::new(on, DepKind::Dev));
        self
    }

    pub fn dev_dep_at(mut self, on: &str, path: &str) -> Self {
        self.deps.push(Dep {
            path: Some(path.to_string()),
            ..Dep::new(on, DepKind::Dev)
        });
        self
    }

    pub fn build_dep_at(mut self, on: &str, path: &str) -> Self {
        self.deps.push(Dep {
            path: Some(path.to_string()),
            ..Dep::new(on, DepKind::Build)
        });
        self
    }

    pub fn optional_dep(mut self, on: &str) -> Self {
        self.deps.push(Dep {
            optional: true,
            ..Dep::new(on, DepKind::Normal)
        });
        self
    }

    pub fn feature(mut self, name: &str, enables: &[&str]) -> Self {
        self.features.push((
            name.to_string(),
            enables.iter().map(|e| e.to_string()).collect(),
        ));
        self
    }

    pub fn file(mut self, rel: &str, contents: &str) -> Self {
        self.files.insert(rel.to_string(), contents.to_string());
        self
    }

    pub fn manifest_extra(mut self, extra: &str) -> Self {
        self.manifest_extra.push(extra.to_string());
        self
    }

    pub fn lib_source(mut self, contents: &str) -> Self {
        self.lib = Some(contents.to_string());
        self
    }

    fn ident(&self) -> String {
        self.name.replace('-', "_")
    }

    fn default_lib(&self) -> String {
        format!("pub fn {}() -> u32 {{ 1 }}\n", self.ident())
    }

    fn manifest(&self) -> String {
        let mut manifest = format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            self.name
        );
        let mut by_table: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for dep in &self.deps {
            by_table
                .entry(dep.kind.manifest_table())
                .or_default()
                .push(dep.manifest_entry());
        }
        for (table, entries) in by_table {
            manifest.push_str(&format!("\n[{table}]\n"));
            for entry in entries {
                manifest.push_str(&entry);
                manifest.push('\n');
            }
        }
        if !self.features.is_empty() {
            manifest.push_str("\n[features]\n");
            for (name, enables) in &self.features {
                let values = enables
                    .iter()
                    .map(|e| format!("\"{e}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                manifest.push_str(&format!("{name} = [{values}]\n"));
            }
        }
        for extra in &self.manifest_extra {
            manifest.push('\n');
            manifest.push_str(extra);
            manifest.push('\n');
        }
        manifest
    }
}

#[derive(Clone, Debug)]
pub struct Workspace {
    pub name: String,
    pub packages: Vec<Package>,
    pub files: BTreeMap<String, String>,
    pub lockfile: bool,
}

pub fn ws(name: &str) -> Workspace {
    Workspace {
        name: name.to_string(),
        packages: Vec::new(),
        files: BTreeMap::new(),
        lockfile: false,
    }
}

impl Workspace {
    pub fn package(mut self, package: Package) -> Self {
        self.packages.push(package);
        self
    }

    /// Generates and commits a real `Cargo.lock` for this workspace so
    /// lockfile-change scenarios start from an authoritative lock.
    pub fn lockfile(mut self) -> Self {
        self.lockfile = true;
        self
    }

    pub fn file(mut self, rel: &str, contents: &str) -> Self {
        self.files.insert(rel.to_string(), contents.to_string());
        self
    }

    fn manifest(&self) -> String {
        let members = self
            .packages
            .iter()
            .map(|package| format!("\"{}\"", package.name))
            .collect::<Vec<_>>()
            .join(", ");
        format!("[workspace]\nmembers = [{members}]\nresolver = \"2\"\n")
    }
}

/// A synthetic multi-workspace Cargo monorepo in a temporary git repository.
pub struct Monorepo {
    dir: TempDir,
    spec: Vec<Workspace>,
    authored: BTreeSet<String>,
    repo: Repository,
}

impl Monorepo {
    pub fn new(spec: Vec<Workspace>) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let repo = Repository::init_opts(
            dir.path(),
            git2::RepositoryInitOptions::new().initial_head("main"),
        )
        .expect("git init");
        let mut seen = BTreeSet::new();
        for workspace in &spec {
            for package in &workspace.packages {
                assert!(
                    seen.insert(package.name.clone()),
                    "package names must be unique across the synthesized monorepo: {}",
                    package.name
                );
            }
        }
        let files = layout(&spec);
        let authored = files.keys().cloned().collect();
        let mut monorepo = Self {
            dir,
            spec,
            authored,
            repo,
        };
        for (rel, contents) in files {
            monorepo.write(&rel, &contents);
        }
        monorepo.generate_lockfiles();
        monorepo
    }

    /// Runs `cargo generate-lockfile` for workspaces that declare a lockfile,
    /// so Cargo treats the committed lock as authoritative in later
    /// metadata runs.
    fn generate_lockfiles(&mut self) {
        let workspaces = self
            .spec
            .iter()
            .filter(|workspace| workspace.lockfile)
            .map(|workspace| workspace.name.clone())
            .collect::<Vec<_>>();
        for name in workspaces {
            let output = Command::new("cargo")
                .arg("generate-lockfile")
                .current_dir(self.dir.path().join(&name))
                .env("PATH", sanitized_path_without_shim())
                .output()
                .expect("run cargo generate-lockfile");
            assert!(
                output.status.success(),
                "generate-lockfile failed for workspace {name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            self.authored.insert(format!("{name}/Cargo.lock"));
        }
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Canonicalized repository root. macOS temp directories live under
    /// `/var`, which canonicalizes to `/private/var`; ferris-wheel reports
    /// absolute workspace paths, so the harness must use one consistent form.
    pub fn canonical_root(&self) -> PathBuf {
        self.dir.path().canonicalize().expect("canonical root")
    }

    pub fn workspace_path(&self, name: &str) -> String {
        self.canonical_root()
            .join(name)
            .to_string_lossy()
            .to_string()
    }

    pub fn spec(&self) -> &[Workspace] {
        &self.spec
    }

    pub fn write(&self, rel: &str, contents: &str) {
        let path = self.dir.path().join(rel);
        fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
        fs::write(path, contents).expect("write file");
    }

    pub fn remove(&self, rel: &str) {
        fs::remove_file(self.dir.path().join(rel)).expect("remove file");
    }

    pub fn rename(&self, from: &str, to: &str) {
        let target = self.dir.path().join(to);
        fs::create_dir_all(target.parent().expect("parent")).expect("create dir");
        fs::rename(self.dir.path().join(from), target).expect("rename file");
    }

    /// Stage everything and commit; returns the commit id.
    ///
    /// `cargo metadata` backfills `Cargo.lock` into workspace roots during
    /// index builds. Those files are generated during a scenario (not
    /// authored by it) and would otherwise leak into the head commit and
    /// the change diff.
    pub fn commit(&self, message: &str) -> String {
        for workspace in &self.spec {
            let relative = format!("{}/Cargo.lock", workspace.name);
            if self.authored.contains(&relative) {
                continue;
            }
            let lockfile = self.dir.path().join(&relative);
            if lockfile.exists() {
                fs::remove_file(lockfile).expect("remove generated lockfile");
            }
        }
        let mut index = self.repo.index().expect("index");
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .expect("stage all");
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = self.repo.find_tree(tree_id).expect("find tree");
        let signature = Signature::new(
            "litmus scenario",
            "scenario@example.com",
            &Time::new(1_700_000_000, 0),
        )
        .expect("signature");
        let parent = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        self.repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &parents,
            )
            .expect("commit")
            .to_string()
    }

    /// Every package as `workspace/package`, with its repository-relative dir.
    pub fn package_dirs(&self) -> BTreeMap<String, String> {
        let mut dirs = BTreeMap::new();
        for workspace in &self.spec {
            for package in &workspace.packages {
                dirs.insert(
                    format!("{}/{}", workspace.name, package.name),
                    format!("{}/{}", workspace.name, package.name),
                );
            }
        }
        dirs
    }

    pub fn workspace_names(&self) -> Vec<String> {
        self.spec.iter().map(|ws| ws.name.clone()).collect()
    }
}

/// All files of the synthesized monorepo, keyed by repository-relative path.
fn layout(spec: &[Workspace]) -> BTreeMap<String, String> {
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    for workspace in spec {
        let base = &workspace.name;
        files.insert(format!("{base}/Cargo.toml"), workspace.manifest());
        for (rel, contents) in &workspace.files {
            files.insert(format!("{base}/{rel}"), contents.clone());
        }
        for package in &workspace.packages {
            let package_dir = format!("{base}/{}", package.name);
            files.insert(format!("{package_dir}/Cargo.toml"), package.manifest());
            let lib_source = package.lib.clone().unwrap_or_else(|| package.default_lib());
            files.insert(format!("{package_dir}/src/lib.rs"), lib_source);
            files.insert(
                format!("{package_dir}/tests/smoke.rs"),
                "#[test]\nfn smoke() {}\n".to_string(),
            );
            for (rel, contents) in &package.files {
                files.insert(format!("{package_dir}/{rel}"), contents.clone());
            }
        }
    }
    files
}
