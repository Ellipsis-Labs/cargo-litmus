//! Scripted `cargo-ferris-wheel`.
//!
//! Litmus invokes `cargo-ferris-wheel` directly; Cargo's external-subcommand
//! dispatch is not a supported path. Scenarios prepend a directory containing a
//! shim script to `PATH`, and the shim replays the payload the harness computed
//! from the synthesized monorepo's dependency graph, matching upstream
//! ferris-wheel's reverse-transitive closure semantics.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::env::sanitized_path_entries;
use super::monorepo::Monorepo;

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
pub(crate) fn ferris_payload(monorepo: &Monorepo, changed_files: &[String]) -> FerrisPayload {
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

pub(crate) fn lineup_payload(monorepo: &Monorepo) -> serde_json::Value {
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
pub(crate) fn write_cargo_wrapper(bin_dir: &Path) {
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
pub(crate) fn ferris_shim_dir(dir: &Path) -> PathBuf {
    let bin_dir = dir.join("ferris-bin");
    fs::create_dir_all(&bin_dir).expect("create shim dir");
    bin_dir
}

/// Writes the scripted `cargo-ferris-wheel` shim into `bin_dir`.
pub(crate) fn write_ferris_shim(
    bin_dir: &Path,
    ripples: &FerrisPayload,
    lineup: &serde_json::Value,
) {
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
