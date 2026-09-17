//! Scenario driver.
//!
//! Commits the synthesized base state, builds the base index, commits the
//! change, runs the real `cargo-litmus affected` binary, and asserts the
//! resulting report against the scenario's expectations.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

use super::env::{litmus_binary, sanitized_path};
use super::expectations::{Expect, assert_expectations};
use super::ferris::{
    FerrisPayload, ferris_payload, ferris_shim_dir, lineup_payload, write_cargo_wrapper,
    write_ferris_shim,
};
use super::monorepo::Monorepo;
use super::report::Report;

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
