//! End-to-end scenario harness for cargo-litmus.
//!
//! Scenarios synthesize arbitrary multi-workspace Rust monorepos, commit a base
//! state, build a base index, then commit a change and run the real
//! `cargo-litmus affected` binary against the repository.
//!
//! ferris-wheel is scripted: the harness writes a `cargo-ferris-wheel` shim on
//! `PATH` that reports the reverse-transitive dependency closure of the changed
//! packages, matching upstream ferris-wheel semantics. That keeps scenarios
//! deterministic and independent of an installed ferris-wheel, while the
//! dependency data still comes from real Cargo manifests parsed by real
//! `cargo metadata`. Scenarios that need deliberately imperfect ferris data can
//! override the payload.
//!
//! Scenario expectations are two-sided:
//! - `required`: packages whose tests MUST be covered (false negatives fail),
//! - `allowed`: packages that MAY be covered (over-selection beyond this
//!   fails).

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{fmt, fs};

use git2::{IndexAddOption, Repository, Signature, Time};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Monorepo synthesis
// ---------------------------------------------------------------------------

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
    pub test_target: bool,
}

pub fn pkg(name: &str) -> Package {
    Package {
        name: name.to_string(),
        deps: Vec::new(),
        features: Vec::new(),
        files: BTreeMap::new(),
        manifest_extra: Vec::new(),
        lib: None,
        test_target: true,
    }
}

impl Package {
    pub fn dependency(mut self, dep: Dep) -> Self {
        self.deps.push(dep);
        self
    }

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

    pub fn build_dep(mut self, on: &str) -> Self {
        self.deps.push(Dep::new(on, DepKind::Build));
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

    pub fn without_test_target(mut self) -> Self {
        self.test_target = false;
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
    pub manifest_extra: Vec<String>,
    pub lockfile: bool,
}

pub fn ws(name: &str) -> Workspace {
    Workspace {
        name: name.to_string(),
        packages: Vec::new(),
        files: BTreeMap::new(),
        manifest_extra: Vec::new(),
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

    pub fn manifest_extra(mut self, extra: &str) -> Self {
        self.manifest_extra.push(extra.to_string());
        self
    }

    fn manifest(&self) -> String {
        let members = self
            .packages
            .iter()
            .map(|package| format!("\"{}\"", package.name))
            .collect::<Vec<_>>()
            .join(", ");
        let mut manifest = format!("[workspace]\nmembers = [{members}]\nresolver = \"2\"\n");
        for extra in &self.manifest_extra {
            manifest.push('\n');
            manifest.push_str(extra);
            manifest.push('\n');
        }
        manifest
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

    pub fn root_file(mut self, rel: &str, contents: &str) -> Self {
        self.authored.insert(rel.to_string());
        self.write(rel, contents);
        self
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
            if package.test_target {
                files.insert(
                    format!("{package_dir}/tests/smoke.rs"),
                    "#[test]\nfn smoke() {}\n".to_string(),
                );
            }
            for (rel, contents) in &package.files {
                files.insert(format!("{package_dir}/{rel}"), contents.clone());
            }
        }
    }
    files
}

// ---------------------------------------------------------------------------
// Scripted ferris-wheel
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct FerrisCrate {
    pub name: String,
    pub workspace: String,
    pub is_directly_affected: bool,
    pub is_standalone: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct FerrisWorkspace {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FerrisPayload {
    pub affected_crates: Vec<FerrisCrate>,
    pub affected_workspaces: Vec<FerrisWorkspace>,
    pub directly_affected_crates: Vec<String>,
    pub directly_affected_workspaces: Vec<FerrisWorkspace>,
}

/// Reverse-transitive closure over every dependency kind, matching the
/// semantics observed from upstream cargo-ferris-wheel.
pub fn ferris_payload(monorepo: &Monorepo, changed_files: &[String]) -> FerrisPayload {
    let package_dirs = monorepo.package_dirs();
    let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for workspace in monorepo.spec() {
        for package in &workspace.packages {
            let key = format!("{}/{}", workspace.name, package.name);
            let edges = package
                .deps
                .iter()
                .map(|dep| {
                    let workspace_name = monorepo
                        .spec()
                        .iter()
                        .find(|ws| ws.packages.iter().any(|p| p.name == dep.on))
                        .map(|ws| ws.name.clone())
                        .unwrap_or_default();
                    format!("{workspace_name}/{}", dep.on)
                })
                .collect::<Vec<_>>();
            deps.insert(key, edges);
        }
    }

    let mut direct = BTreeSet::new();
    for file in changed_files {
        let mut best: Option<(&String, usize)> = None;
        for (key, dir) in &package_dirs {
            let prefix = format!("{dir}/");
            if (file == dir || file.starts_with(&prefix))
                && best.is_none_or(|(_, len)| prefix.len() > len)
            {
                best = Some((key, prefix.len()));
            }
        }
        if let Some((key, _)) = best {
            direct.insert(key.clone());
        }
    }

    let mut affected = direct.clone();
    loop {
        let mut grew = false;
        for (package, dependencies) in &deps {
            if affected.contains(package) {
                continue;
            }
            if dependencies.iter().any(|dep| affected.contains(dep)) {
                affected.insert(package.clone());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    let workspace_of = |key: &str| key.split('/').next().unwrap_or_default().to_string();
    let mut affected_workspaces = BTreeSet::new();
    for key in &affected {
        affected_workspaces.insert(workspace_of(key));
    }
    let mut directly_affected_workspaces = BTreeSet::new();
    for key in &direct {
        directly_affected_workspaces.insert(workspace_of(key));
    }

    let root = monorepo.canonical_root();
    let workspace_payload = |name: &str| FerrisWorkspace {
        name: name.to_string(),
        path: root.join(name).to_string_lossy().to_string(),
    };

    FerrisPayload {
        affected_crates: affected
            .iter()
            .map(|key| FerrisCrate {
                name: key.split('/').nth(1).unwrap_or_default().to_string(),
                workspace: workspace_of(key),
                is_directly_affected: direct.contains(key),
                is_standalone: false,
            })
            .collect(),
        affected_workspaces: affected_workspaces
            .iter()
            .map(|name| workspace_payload(name))
            .collect(),
        directly_affected_crates: direct
            .iter()
            .map(|key| key.split('/').nth(1).unwrap_or_default().to_string())
            .collect(),
        directly_affected_workspaces: directly_affected_workspaces
            .iter()
            .map(|name| workspace_payload(name))
            .collect(),
    }
}

fn lineup_payload(monorepo: &Monorepo) -> serde_json::Value {
    let workspaces = monorepo
        .workspace_names()
        .into_iter()
        .map(|name| {
            serde_json::json!({
                "name": name,
                "path": monorepo.workspace_path(&name),
                "dependencies": [],
                "reverse": false,
                "transitive": false,
                "is_standalone": false,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({ "workspaces": workspaces })
}

/// Writes a `cargo` wrapper whose external-subcommand dispatch fails, the way a
/// build-cache wrapper behaves when it cannot verify the Cargo build storage.
/// Every other cargo invocation is forwarded to the real toolchain.
fn write_cargo_wrapper(bin_dir: &Path) {
    let real_cargo = sanitized_path_entries()
        .into_iter()
        .map(|directory| directory.join("cargo"))
        .find(|candidate| candidate.is_file())
        .expect("real cargo on PATH");
    let script = bin_dir.join("cargo");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"ferris-wheel\" ]; then\n  echo \"wrapped cargo: could not \
             verify Cargo build storage\" >&2\n  exit 1\nfi\nexec \"{}\" \"$@\"\n",
            real_cargo.display()
        ),
    )
    .expect("write cargo wrapper");
    let mut permissions = fs::metadata(&script)
        .expect("cargo wrapper metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("chmod cargo wrapper");
}

/// Directory prepended to `PATH` so litmus resolves `cargo-ferris-wheel` to the
/// scripted shim.
fn ferris_shim_dir(dir: &Path) -> PathBuf {
    let bin_dir = dir.join("ferris-bin");
    fs::create_dir_all(&bin_dir).expect("create shim dir");
    bin_dir
}

/// Writes the scripted `cargo-ferris-wheel` shim into `bin_dir`.
fn write_ferris_shim(bin_dir: &Path, ripples: &FerrisPayload, lineup: &serde_json::Value) {
    write_ferris_payloads(bin_dir, ripples, lineup);
    write_shim_script(bin_dir, DIRECT_SHIM);
}

/// Litmus invokes `cargo-ferris-wheel` directly; Cargo's external-subcommand
/// dispatch is not a supported path. The shim rejects dispatch so the scenarios
/// keep that contract honest.
const DIRECT_SHIM: &str = r#"#!/bin/sh
for argument in "$@"; do
  if [ "$argument" = "ferris-wheel" ]; then
    echo "scenario shim: cargo dispatch is not supported" >&2
    exit 65
  fi
done
for argument in "$@"; do
  if [ "$argument" = "ripples" ]; then
    exec cat "@BIN@/ripples.json"
  fi
done
exec cat "@BIN@/lineup.json"
"#;

fn write_ferris_payloads(bin_dir: &Path, ripples: &FerrisPayload, lineup: &serde_json::Value) {
    fs::write(
        bin_dir.join("ripples.json"),
        serde_json::to_vec_pretty(ripples).expect("serialize ripples"),
    )
    .expect("write ripples");
    fs::write(
        bin_dir.join("lineup.json"),
        serde_json::to_vec_pretty(lineup).expect("serialize lineup"),
    )
    .expect("write lineup");
}

fn write_shim_script(bin_dir: &Path, template: &str) {
    let path = bin_dir.join("cargo-ferris-wheel");
    fs::write(
        &path,
        template.replace("@BIN@", &bin_dir.display().to_string()),
    )
    .expect("write shim");
    let mut permissions = fs::metadata(&path).expect("shim metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("chmod shim");
}

/// A `PATH` that keeps the real toolchain but drops shell-manager wrappers
/// (mise/mbx) that break `cargo metadata` in manifest-less monorepo roots.
fn sanitized_path_entries() -> Vec<PathBuf> {
    let current = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&current)
        .filter(|path| !path.to_string_lossy().contains("/mise/"))
        .collect()
}

fn sanitized_path_without_shim() -> std::ffi::OsString {
    std::env::join_paths(sanitized_path_entries()).expect("join PATH")
}

fn sanitized_path(shim_dir: &Path) -> std::ffi::OsString {
    let mut paths = sanitized_path_entries();
    paths.insert(0, shim_dir.to_path_buf());
    std::env::join_paths(paths).expect("join PATH")
}

fn litmus_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cargo-litmus"))
}

// ---------------------------------------------------------------------------
// Report model (test-side mirror of the stable JSON contract)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub failed_wide: bool,
    pub selection_reason: String,
    pub test_failed_wide: bool,
    pub test_selection_reason: String,
    pub unknown_files: Vec<String>,
    pub changed_files: Vec<ChangedFile>,
    pub affected_crates: Vec<CrateRef>,
    pub affected_workspaces: Vec<WorkspaceRef>,
    pub directly_affected_crates: Vec<String>,
    pub affected_test_crates: Vec<CrateRef>,
    pub selected_test_packages: Vec<SelectedPackage>,
    pub nextest_commands: Vec<NextestCommand>,
    pub test_workspace_selections: Vec<WorkspaceSelection>,
    pub test_selection_checks: Vec<SelectionCheck>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CrateRef {
    pub name: String,
    pub workspace: String,
    pub is_directly_affected: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkspaceRef {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SelectedPackage {
    pub name: String,
    pub workspace: String,
    pub reason: String,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandMode {
    Module,
    TestTarget,
    Package,
    WorkspaceWide,
}

#[derive(Clone, Debug, Deserialize)]
pub struct NextestCommand {
    pub workspace: String,
    pub args: Vec<String>,
    pub mode: CommandMode,
    pub module_filter: Option<String>,
    pub widened_from: Option<CommandMode>,
    pub reason: String,
    #[serde(default)]
    pub matched_tests: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkspaceSelection {
    pub workspace: WorkspaceRef,
    pub failed_wide: bool,
    pub selection_reason: String,
    pub unknown_files: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SelectionCheck {
    pub changed_package: PackageRef,
    pub missing_dependents: Vec<PackageRef>,
    pub result: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PackageRef {
    pub name: String,
    pub workspace: String,
}

impl NextestCommand {
    /// Every `-p <package>` argument; litmus batches multiple packages into one
    /// nextest invocation per workspace.
    pub fn package_args(&self) -> Vec<&str> {
        self.args
            .windows(2)
            .filter(|window| window[0] == "-p")
            .map(|window| window[1].as_str())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Scenario driver
// ---------------------------------------------------------------------------

/// A `workspace/package` expectation target.
pub fn target(spec: &str) -> String {
    spec.to_string()
}

pub struct Expect {
    required: BTreeSet<String>,
    allowed: BTreeSet<String>,
    required_commands: Vec<CommandExpectation>,
    forbid_global_fail_wide: bool,
    note: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CommandExpectation {
    pub workspace: String,
    pub mode: CommandMode,
    pub package: Option<String>,
    pub module_filter: Option<String>,
}

impl CommandExpectation {
    pub fn new(workspace: &str, mode: CommandMode) -> Self {
        Self {
            workspace: workspace.to_string(),
            mode,
            package: None,
            module_filter: None,
        }
    }

    pub fn package(mut self, package: &str) -> Self {
        self.package = Some(package.to_string());
        self
    }

    pub fn module_filter(mut self, filter: &str) -> Self {
        self.module_filter = Some(filter.to_string());
        self
    }
}

impl Expect {
    /// Every listed package must be covered, and nothing else may be covered.
    pub fn exact<S: AsRef<str>>(packages: &[S]) -> Self {
        let set: BTreeSet<String> = packages.iter().map(|p| p.as_ref().to_string()).collect();
        Self {
            required: set.clone(),
            allowed: set,
            required_commands: Vec::new(),
            forbid_global_fail_wide: false,
            note: None,
        }
    }

    /// `required` must be covered; anything outside `allowed` fails.
    pub fn bounded<S: AsRef<str>, T: AsRef<str>>(required: &[S], allowed: &[T]) -> Self {
        Self {
            required: required.iter().map(|p| p.as_ref().to_string()).collect(),
            allowed: allowed.iter().map(|p| p.as_ref().to_string()).collect(),
            required_commands: Vec::new(),
            forbid_global_fail_wide: false,
            note: None,
        }
    }

    pub fn require_command(mut self, expectation: CommandExpectation) -> Self {
        self.required_commands.push(expectation);
        self
    }

    pub fn forbid_global_fail_wide(mut self) -> Self {
        self.forbid_global_fail_wide = true;
        self
    }

    pub fn note(mut self, note: &str) -> Self {
        self.note = Some(note.to_string());
        self
    }
}

pub enum Change {
    Edit { path: String, contents: String },
    Create { path: String, contents: String },
    Delete { path: String },
    Rename { from: String, to: String },
}

pub fn edit(path: &str, contents: &str) -> Change {
    Change::Edit {
        path: path.to_string(),
        contents: contents.to_string(),
    }
}

pub fn create(path: &str, contents: &str) -> Change {
    Change::Create {
        path: path.to_string(),
        contents: contents.to_string(),
    }
}

pub fn delete(path: &str) -> Change {
    Change::Delete {
        path: path.to_string(),
    }
}

pub fn rename(from: &str, to: &str) -> Change {
    Change::Rename {
        from: from.to_string(),
        to: to.to_string(),
    }
}

/// Runs one scenario end to end and panics with a diagnostic report on any
/// expectation violation.
pub fn check(name: &str, monorepo: Monorepo, changes: &[Change], expect: &Expect) -> Report {
    run_scenario(name, monorepo, changes, expect, RunOptions::default())
}

/// Like [`check`], but the ferris-wheel report can be scripted explicitly
/// (for example to model an under-reporting ferris-wheel).
pub fn check_with_ferris(
    name: &str,
    monorepo: Monorepo,
    changes: &[Change],
    expect: &Expect,
    ferris_override: Option<FerrisPayload>,
) -> Report {
    run_scenario(
        name,
        monorepo,
        changes,
        expect,
        RunOptions {
            ferris_override,
            ..RunOptions::default()
        },
    )
}

/// Like [`check`], but `cargo ferris-wheel` dispatch fails the way it does when
/// `cargo` is wrapped by a build cache such as mbx. Litmus must still select
/// narrowly by invoking the `cargo-ferris-wheel` binary directly.
pub fn check_with_wrapped_cargo(
    name: &str,
    monorepo: Monorepo,
    changes: &[Change],
    expect: &Expect,
) -> Report {
    run_scenario(
        name,
        monorepo,
        changes,
        expect,
        RunOptions {
            wrap_cargo: true,
            ..RunOptions::default()
        },
    )
}

#[derive(Default)]
struct RunOptions {
    ferris_override: Option<FerrisPayload>,
    wrap_cargo: bool,
}

fn run_scenario(
    name: &str,
    monorepo: Monorepo,
    changes: &[Change],
    expect: &Expect,
    options: RunOptions,
) -> Report {
    let base = monorepo.commit("base");
    let scratch = TempDir::new().expect("scratch dir");
    let base_cache = scratch.path().join("base.rkyv");
    let current_cache = scratch.path().join("current.rkyv");
    let bin_dir = ferris_shim_dir(scratch.path());

    let base_output = run_litmus(
        &monorepo,
        &bin_dir,
        &["index", "--cache", base_cache.to_str().expect("utf8")],
    );
    assert!(
        base_output.status.success(),
        "scenario {name}: base index failed: {}",
        String::from_utf8_lossy(&base_output.stderr)
    );

    let changed_paths = apply_changes(&monorepo, changes);
    let head = monorepo.commit("head");

    let payload = options
        .ferris_override
        .unwrap_or_else(|| ferris_payload(&monorepo, &changed_paths));
    write_ferris_shim(&bin_dir, &payload, &lineup_payload(&monorepo));
    if options.wrap_cargo {
        write_cargo_wrapper(&bin_dir);
    }

    let output = run_litmus(
        &monorepo,
        &bin_dir,
        &[
            "affected",
            "--base",
            &base,
            "--head",
            &head,
            "--base-cache",
            base_cache.to_str().expect("utf8"),
            "--current-cache",
            current_cache.to_str().expect("utf8"),
            "--format",
            "json",
        ],
    );

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "scenario {name}: litmus affected failed (status {:?})\nstdout:\n{stdout}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Report = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!("scenario {name}: report is not valid JSON: {error}\n{stdout}")
    });

    assert_expectations(name, &monorepo, &report, expect);
    report
}

fn apply_changes(monorepo: &Monorepo, changes: &[Change]) -> Vec<String> {
    let mut paths = Vec::new();
    for change in changes {
        match change {
            Change::Edit { path, contents } | Change::Create { path, contents } => {
                monorepo.write(path, contents);
                paths.push(path.clone());
            }
            Change::Delete { path } => {
                monorepo.remove(path);
                paths.push(path.clone());
            }
            Change::Rename { from, to } => {
                monorepo.rename(from, to);
                paths.push(from.clone());
                paths.push(to.clone());
            }
        }
    }
    paths
}

fn run_litmus(monorepo: &Monorepo, bin_dir: &Path, args: &[&str]) -> std::process::Output {
    let root = monorepo.canonical_root();
    Command::new(litmus_binary())
        .args(args)
        .arg("--root")
        .arg(&root)
        .current_dir(&root)
        .env("PATH", sanitized_path(bin_dir))
        .env_remove("LITMUS_BASE_CACHE")
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .expect("run cargo-litmus")
}

// ---------------------------------------------------------------------------
// Assertions and diagnostics
// ---------------------------------------------------------------------------

fn covered_packages(monorepo: &Monorepo, report: &Report) -> BTreeSet<String> {
    let mut covered = BTreeSet::new();
    for command in &report.nextest_commands {
        match command.mode {
            CommandMode::WorkspaceWide => {
                for workspace in monorepo.spec() {
                    if workspace.name == command.workspace {
                        for package in &workspace.packages {
                            covered.insert(format!("{}/{}", workspace.name, package.name));
                        }
                    }
                }
            }
            _ => {
                for package in command.package_args() {
                    covered.insert(format!("{}/{}", command.workspace, package));
                }
            }
        }
    }
    covered
}

fn assert_expectations(name: &str, monorepo: &Monorepo, report: &Report, expect: &Expect) {
    let covered = covered_packages(monorepo, report);
    let required: BTreeSet<String> = expect.required.clone();
    let allowed: BTreeSet<String> = expect.allowed.clone();

    let mut violations = Vec::new();
    for missing in required.difference(&covered) {
        violations.push(format!(
            "false negative: required package {missing} is not covered"
        ));
    }
    for extra in covered.difference(&allowed) {
        violations.push(format!(
            "over-selection: package {extra} is covered but not allowed"
        ));
    }
    if expect.forbid_global_fail_wide && report.failed_wide {
        violations.push(format!(
            "unexpected global fail-wide: {}",
            report.selection_reason
        ));
    }
    for expected in &expect.required_commands {
        let matched = report.nextest_commands.iter().any(|command| {
            command.workspace == expected.workspace
                && command.mode == expected.mode
                && expected
                    .package
                    .as_ref()
                    .is_none_or(|package| command.package_args().contains(&package.as_str()))
                && expected
                    .module_filter
                    .as_ref()
                    .is_none_or(|filter| command.module_filter.as_deref() == Some(filter.as_str()))
        });
        if !matched {
            violations.push(format!(
                "missing required command: workspace={} mode={:?} package={:?} module_filter={:?}",
                expected.workspace, expected.mode, expected.package, expected.module_filter
            ));
        }
    }

    if violations.is_empty() {
        return;
    }

    let mut diagnostic = format!("scenario {name} failed expectations:\n");
    for violation in &violations {
        diagnostic.push_str(&format!("  - {violation}\n"));
    }
    if let Some(note) = &expect.note {
        diagnostic.push_str(&format!("  note: {note}\n"));
    }
    diagnostic.push_str(&format!(
        "\n  failed_wide={} selection_reason={:?}\n  test_failed_wide={} \
         test_selection_reason={:?}\n",
        report.failed_wide,
        report.selection_reason,
        report.test_failed_wide,
        report.test_selection_reason
    ));
    diagnostic.push_str(&format!("  unknown_files={:?}\n", report.unknown_files));
    diagnostic.push_str(&format!(
        "  changed_files={:?}\n",
        report
            .changed_files
            .iter()
            .map(|file| format!("{}:{}", file.path, file.status))
            .collect::<Vec<_>>()
    ));
    diagnostic.push_str("  selected_test_packages:\n");
    for package in &report.selected_test_packages {
        diagnostic.push_str(&format!(
            "    {}/{} ({})\n",
            package.workspace, package.name, package.reason
        ));
    }
    diagnostic.push_str("  nextest_commands:\n");
    for command in &report.nextest_commands {
        diagnostic.push_str(&format!(
            "    workspace={} mode={:?} args={:?} module_filter={:?} widened_from={:?} \
             reason={:?}\n",
            command.workspace,
            command.mode,
            command.args,
            command.module_filter,
            command.widened_from,
            command.reason
        ));
    }
    diagnostic.push_str("  test_workspace_selections:\n");
    for selection in &report.test_workspace_selections {
        diagnostic.push_str(&format!(
            "    {} failed_wide={} reason={:?} unknown_files={:?}\n",
            selection.workspace.name,
            selection.failed_wide,
            selection.selection_reason,
            selection.unknown_files
        ));
    }
    if !report.test_selection_checks.is_empty() {
        diagnostic.push_str("  test_selection_checks:\n");
        for check in &report.test_selection_checks {
            diagnostic.push_str(&format!(
                "    changed={}/{} result={} missing_dependents={:?}\n",
                check.changed_package.workspace,
                check.changed_package.name,
                check.result,
                check
                    .missing_dependents
                    .iter()
                    .map(|package| format!("{}/{}", package.workspace, package.name))
                    .collect::<Vec<_>>()
            ));
        }
    }
    panic!("{diagnostic}");
}

impl fmt::Display for CommandMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}
