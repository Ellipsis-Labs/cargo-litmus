//! `PATH` and binary resolution for scenario subprocesses.
//!
//! Scenario subprocesses must see the real toolchain plus the scripted
//! ferris-wheel shim directory, and nothing else that could interfere.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// A `PATH` that keeps the real toolchain but drops shell-manager wrappers
/// (mise/mbx) that break `cargo metadata` in manifest-less monorepo roots.
pub(crate) fn sanitized_path_entries() -> Vec<PathBuf> {
    let current = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&current)
        .filter(|path| !path.to_string_lossy().contains("/mise/"))
        .collect()
}

pub(crate) fn sanitized_path_without_shim() -> OsString {
    std::env::join_paths(sanitized_path_entries()).expect("join PATH")
}

pub(crate) fn sanitized_path(shim_dir: &Path) -> OsString {
    let mut paths = sanitized_path_entries();
    paths.insert(0, shim_dir.to_path_buf());
    std::env::join_paths(paths).expect("join PATH")
}

pub(crate) fn litmus_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cargo-litmus"))
}
