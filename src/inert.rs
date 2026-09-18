//! Built-in classification of changed files that cannot be build or test
//! inputs.
//!
//! A changed file with no indexed relationship to Rust source is normally
//! treated as an unknown input, which widens selection to every workspace.
//! Documentation, media, legal files, CI, agent, and editor metadata, and
//! JavaScript and TypeScript sources are not compiled, executed, or opened by a
//! Cargo build or test: a file that *is* embedded (`include!`, `include_str!`,
//! `include_bytes!`, `#[path]`), configured (`[[rules]]`,
//! `[[build-script-inputs]]`), or read at build time through a declared input
//! is mapped before this classification runs, so those files never reach it.
//!
//! The classification is deliberately narrow: files a build could read,
//! execute, or consume as data (manifests, lockfiles, data files, templates,
//! schemas, fixtures, snapshots, scripts) stay conservative. The one executed
//! exception is JavaScript and TypeScript source: Cargo never compiles, runs,
//! or consumes it, and the toolchain that does is invisible to the index, so
//! the file behaves like documentation. Repositories whose Rust builds or
//! tests do read JavaScript or TypeScript trees map those paths back in with
//! `[[rules]]`, extend the classes with `[[rules]] selection = "ignore"`, and
//! disable the built-in classes entirely with `default-inert-paths = false`.

/// Directory names whose contents are repository metadata: continuous
/// integration, coding agents, and editor configuration. None of them are
/// inputs to a Cargo build.
const METADATA_DIRECTORIES: &[&str] = &[
    ".agents",
    ".claude",
    ".cursor",
    ".devcontainer",
    ".github",
    ".idea",
    ".vscode",
];

/// Either the file name is exactly one of these, or its lowercase name starts
/// with one of them and carries a documentation extension (`LICENSE`,
/// `LICENSE-MIT`, `LICENSE.txt`).
const METADATA_FILE_PREFIXES: &[&str] = &[
    "authors",
    "changelog",
    "changes",
    "codeowners",
    "contributors",
    "contributing",
    "copying",
    "license",
    "licence",
    "notice",
    "security",
];

/// Exact file names that are repository metadata.
const METADATA_FILE_NAMES: &[&str] = &[".cargo-litmus.toml", "codeowners"];

/// Extensions of prose, media, fonts, and documents. Rust source can embed any
/// of them, but an embedded file is indexed and mapped before this check.
const INERT_EXTENSIONS: &[&str] = &[
    "adoc", "avif", "gif", "ico", "jpeg", "jpg", "md", "mdx", "mov", "mp3", "mp4", "otf", "pdf",
    "png", "rst", "svg", "ttf", "wav", "webp", "woff", "woff2",
];

/// Extensions allowed on metadata-prefixed files (`LICENSE.txt`).
const METADATA_FILE_EXTENSIONS: &[&str] = &["adoc", "md", "rst", "txt"];

/// Extensions of JavaScript and TypeScript sources. Cargo never compiles,
/// runs, or consumes them; the toolchain that does is outside the index, and
/// a file a Rust build or test reads is mapped before this check (indexed
/// includes, configured rules).
const JAVASCRIPT_SOURCE_EXTENSIONS: &[&str] =
    &["cjs", "cts", "js", "jsx", "mjs", "mts", "ts", "tsx"];

/// Returns the reason the file cannot be an input to a build or test.
pub(crate) fn classify(path: &str) -> Option<&'static str> {
    if path.split('/').any(in_metadata_directory) {
        return Some("continuous integration, agent, and editor metadata is not a build input");
    }
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let lower = file_name.to_ascii_lowercase();
    if is_container_recipe(&lower) {
        return Some("container build recipes are not build or test inputs for Cargo");
    }
    if METADATA_FILE_NAMES.contains(&lower.as_str()) {
        return Some("repository metadata is not a build input");
    }
    let extension = lower.rsplit_once('.').map(|(_, extension)| extension);
    if extension.is_some_and(|extension| INERT_EXTENSIONS.contains(&extension)) {
        return Some("documentation, media, and fonts are not build inputs unless indexed");
    }
    if extension.is_some_and(|extension| JAVASCRIPT_SOURCE_EXTENSIONS.contains(&extension)) {
        return Some("JavaScript and TypeScript sources are not built or run by Cargo");
    }
    if metadata_prefix_matches(&lower, extension) {
        return Some("legal, release, and community files are not build inputs");
    }
    None
}

fn metadata_prefix_matches(lower: &str, extension: Option<&str>) -> bool {
    if !METADATA_FILE_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return false;
    }
    extension.is_none_or(|extension| METADATA_FILE_EXTENSIONS.contains(&extension))
}

fn in_metadata_directory(component: &str) -> bool {
    METADATA_DIRECTORIES.contains(&component)
}

fn is_container_recipe(lower: &str) -> bool {
    lower == "dockerfile"
        || lower.starts_with("dockerfile.")
        || lower == ".dockerignore"
        || lower.ends_with(".dockerfile")
        || (lower.starts_with("docker-compose")
            && (lower.ends_with(".yml") || lower.ends_with(".yaml")))
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn classifies_documentation_and_media() {
        assert!(classify("docs/notes.md").is_some());
        assert!(classify("README.md").is_some());
        assert!(classify("assets/hero.png").is_some());
        assert!(classify("LICENSE").is_some());
        assert!(classify("LICENSE-MIT").is_some());
        assert!(classify("LICENSE.txt").is_some());
        assert!(classify("CHANGELOG.md").is_some());
        assert!(classify("crates/api/README.md").is_some());
    }

    #[test]
    fn classifies_repository_metadata() {
        assert!(classify(".github/workflows/ci.yaml").is_some());
        assert!(classify(".github/actions/setup/action.yml").is_some());
        assert!(classify(".agents/skills/review/SKILL.md").is_some());
        assert!(classify(".vscode/settings.json").is_some());
        assert!(classify(".cargo-litmus.toml").is_some());
        assert!(classify("CODEOWNERS").is_some());
        assert!(classify("docker-compose.dev.yml").is_some());
        assert!(classify("ts/dashboard/Dockerfile.ci").is_some());
    }

    #[test]
    fn classifies_javascript_and_typescript_sources() {
        assert!(classify("web/src/main.ts").is_some());
        assert!(classify("ts/dashboard/src/app.tsx").is_some());
        assert!(classify("rise/ts/src/api/client.js").is_some());
        assert!(classify("scripts/report.mjs").is_some());
        assert!(classify("tools/legacy/run.cjs").is_some());
        assert!(classify("web/src/types.d.ts").is_some());
    }

    #[test]
    fn keeps_source_and_data_conservative() {
        assert!(classify("configs/market.toml").is_none());
        assert!(classify("dev-config.yaml").is_none());
        assert!(classify("programs/idl/ember.json").is_none());
        assert!(classify("ts/idl-tool/package.json").is_none());
        assert!(classify("localnet-config.toml").is_none());
        assert!(classify("nodes/db/migrations/0001_init.sql").is_none());
        assert!(classify("scripts/verify-idl.py").is_none());
        assert!(classify("mise.toml").is_none());
        assert!(classify("tests/fixtures/expected.snap").is_none());
        assert!(classify("crates/api/tests/expected.txt").is_none());
    }

    #[test]
    fn keeps_source_files_with_metadata_prefixes_conservative() {
        assert!(classify("crates/api/src/license.rs").is_none());
        assert!(classify("crates/api/src/authors.rs").is_none());
        assert!(classify("crates/api/src/security/mod.rs").is_none());
        assert!(classify("crates/api/src/changes.rs").is_none());
        assert!(classify("crates/api/Cargo.toml").is_none());
        assert!(classify("crates/api/build.rs").is_none());
    }
}
