[![Docs](https://docs.rs/cargo-litmus/badge.svg)](https://docs.rs/cargo-litmus) [![Crates.io](https://img.shields.io/crates/v/cargo-litmus)](https://crates.io/crates/cargo-litmus) [![Build & test](https://github.com/Ellipsis-Labs/cargo-litmus/actions/workflows/build.yaml/badge.svg)](https://github.com/Ellipsis-Labs/cargo-litmus/actions/workflows/build.yaml)

# cargo-litmus

![A glowing test train routed through the affected path in a Rust dependency rail yard](assets/cargo-litmus-hero.png)

Select the Rust tests affected by a Git change and emit focused `cargo nextest` commands.

`cargo-litmus` indexes Rust source relationships, combines them with Cargo package and workspace dependency graphs from [`cargo-ferris-wheel`](https://github.com/Ellipsis-Labs/cargo-ferris-wheel), and narrows runs to affected modules, test targets, or packages when safe. Selection is conservative: when Litmus cannot prove a narrow filter is complete, it widens to the owning package, workspace, or repository instead of risking a false negative.

## Install

`cargo-litmus` requires [`cargo-ferris-wheel`](https://github.com/Ellipsis-Labs/cargo-ferris-wheel) 1.1.4 or newer on `PATH`. Litmus invokes the `cargo-ferris-wheel` binary directly so workspace discovery works from monorepo roots without a root `Cargo.toml` and in environments where `cargo` is wrapped by a build cache such as `mbx`.

[`cargo-nextest`](https://nexte.st/) is optional unless you execute the emitted commands or pass `--validate-nextest`. The minimum supported Rust version is 1.88.0.

```bash
# Prebuilt binaries via cargo-binstall.
cargo binstall cargo-litmus cargo-ferris-wheel cargo-nextest

# Or build the required binaries from source.
cargo install cargo-litmus cargo-ferris-wheel
```

## Quick start

Run from a Git repository (or subdirectory) containing one or more Cargo workspaces:

```bash
# Build or update target/cargo-litmus.rkyv.
cargo litmus index --root .

# Select tests for changes between the merge-base of origin/main and HEAD.
cargo litmus affected --root . --merge-base origin/main

# Select tests for staged, unstaged, deleted, renamed, and untracked worktree changes.
cargo litmus affected --root . --worktree

# Explain why a path or package selects tests and where selection widens.
cargo litmus explain crates/api/src/lib.rs --root .
cargo litmus explain --package api-server --root .
```

Default `summary` output prints selection and cache telemetry. Pass `--format json` to print the full machine-readable report to stdout, or `--json-output <path>` to write the JSON report to disk while keeping summary telemetry on stdout.

## Selecting changes

`affected` accepts explicit file lists or Git revisions:

```bash
# Explicit repository-relative paths.
cargo litmus affected --files crates/api/src/lib.rs crates/types/src/lib.rs

# Exact revision range, including changed line ranges.
cargo litmus affected --base origin/main --head HEAD

# Staged index changes only.
cargo litmus affected --staged

# Newline-delimited paths, a JSON array, or a JSON object on stdin.
git diff --name-only origin/main...HEAD | cargo litmus affected --files-stdin
```

`--merge-base`, `--worktree`, `--staged`, and `--base`/`--head` are mutually exclusive. Combine `--files` or `--files-stdin` with `--base` and `--head` to restrict revision-range and changed-line analysis to those paths.

## Index caches

The current index defaults to `$CARGO_TARGET_DIR/cargo-litmus.rkyv`, or `target/cargo-litmus.rkyv` when `CARGO_TARGET_DIR` is unset. Override it with `--cache` (`index`) or `--current-cache` (`affected`, `explain`).

For revision-aware CI, build an index at the base revision and pass it via `--base-cache` (or `LITMUS_BASE_CACHE`):

```bash
base_sha="$(git merge-base origin/main HEAD)"
base_tmp="$(mktemp -d)"
base_worktree="$base_tmp/repo"
trap 'git worktree remove --force "$base_worktree"; rmdir "$base_tmp"' EXIT

git worktree add --detach "$base_worktree" "$base_sha"
cargo litmus index --root "$base_worktree" --cache target/cargo-litmus-base.rkyv
cargo litmus affected \
  --root . \
  --base "$base_sha" \
  --head HEAD \
  --base-cache target/cargo-litmus-base.rkyv \
  --format json
```

Cache metadata records the repository root and source/Cargo fingerprints. Missing, unreadable, or incompatible caches fall back to a conservative rebuild.

Warm analysis reuses the loaded index without reconstructing its item graph when
the supplied changes and repository fingerprints require no update. Adding or
removing Rust sources refreshes Cargo metadata to discover automatic targets.

Index format version 10 rebuilds older caches to include conditional module-path
dependencies; existing base caches should be regenerated for structural comparisons.

## How inputs select tests

Litmus classifies each changed path in precedence order:

1. **Configured inputs**: `.cargo-litmus.toml` rules select named workspaces, packages and reverse dependents, or explicitly ignore paths.
2. **Cargo inputs**: `.cargo/config.toml` (`.cargo/config`) and `rust-toolchain.toml` (`rust-toolchain`) resolve up the directory tree and widen to every workspace. `Cargo.toml`, `Cargo.lock`, and `build.rs` select their owning package and reverse dependents, or widen to the owning workspace when outside any package.
3. **Indexed and embedded sources**: Tracked `.rs` files map to their owning package and reverse dependents. Files referenced by `include!`, `include_str!`, `include_bytes!`, or `#[path]` map to the packages that embed them and their reverse dependents. Conditional `#[cfg_attr(..., path = "...")]` modules include every path alternative, including nested attributes and the default module location, without assuming a particular platform or feature set.
4. **Generated paths** (`target/**`, `**/target/**`, `**/out/**`): Have no static source relationship and widen conservatively.
5. **Built-in inert paths** (outside package roots, when `default-inert-paths = true`): Select nothing and are omitted from `cargo-ferris-wheel` queries, which would otherwise widen on unmapped files. Built-in inert classification stops at package roots: an unconfigured file beneath a package root still selects that package's closure because package code or tests can read files under `CARGO_MANIFEST_DIR` at runtime.
6. **Unmapped files**: Select the owning package closure when beneath a package root, widen to the workspace when inside a workspace outside its packages, or widen to all workspaces when outside every workspace.

Package-local narrowing requires every changed path to qualify. A manifest or
binary-entrypoint change cannot hide a simultaneous library change: dependent
tests remain selected. Lockfile changes retain the affected dependency closure.
Production code stays production regardless of layout: `src/tests/`, a custom
`tests.rs` library, or a `tests/` directory containing a production target.

Built-in inert classes:

| Class | Examples |
|---|---|
| Documentation, media, and fonts | `*.md`, `*.mdx`, `*.rst`, `*.adoc`, `*.png`, `*.svg`, `*.pdf`, `*.woff2` |
| JavaScript and TypeScript sources | `*.ts`, `*.tsx`, `*.js`, `*.jsx`, `*.mjs`, `*.cjs`, `*.mts`, `*.cts` |
| Legal, release, and community | `LICENSE*`, `COPYING*`, `NOTICE*`, `CHANGELOG*`, `CONTRIBUTING*`, `SECURITY*`, `CODEOWNERS`, `.cargo-litmus.toml` |
| CI, agent, and editor metadata | `.github/**`, `.agents/**`, `.claude/**`, `.cursor/**`, `.devcontainer/**`, `.vscode/**`, `.idea/**` |
| Container recipes | `Dockerfile*`, `*.dockerfile`, `.dockerignore`, `docker-compose*.yml`, `docker-compose*.yaml` |

Custom builds, proc macros, or tests may consume otherwise inert files. Map those paths with `[[rules]]` or `[[build-script-inputs]]`, or set `default-inert-paths = false` to make all unconfigured files conservative.

Other data and configuration files (`*.toml`, `*.yaml`, `*.json`, `*.sql`, `*.snap`), scripts, and templates remain conservative. JSON output includes each path's classification in `input_explanations`.

## Configuration (`.cargo-litmus.toml`)

Place `.cargo-litmus.toml` at the analyzed root to map non-Rust inputs, declare build-script inputs, or ignore repository-specific trees:

```toml
default-inert-paths = true

[[rules]]
paths = ["migrations/**"]
workspaces = ["server"]
selection = "workspace"

[[rules]]
paths = ["schemas/**"]
packages = ["api-types", "api-server"]
selection = "packages"

[[rules]]
paths = ["ts/**", "vendor/tooling/**"]
selection = "ignore"

[[build-script-inputs]]
paths = ["codegen/**"]
packages = ["generated-client"]
```

- **Workspace names**: For `selection = "workspace"`, a workspace whose root `Cargo.toml` declares `[package]` uses that package's Cargo name regardless of member count. Virtual workspaces use their root directory name (or `"root"` at the repository root).
- **Globs**: `paths` use `globset` syntax (`*`, `**`, `?`, `[abc]`, `[!abc]`, `{a,b}`) matched against repository-relative paths.
- **Precedence**: A path matched by multiple rules selects targets if *any* matching rule selects workspaces or packages; it is ignored only when *every* matching rule sets `selection = "ignore"`, regardless of rule order.
- **Validation**: Unknown fields, invalid globs, empty path or target lists, contradictory `ignore` targets, and unknown workspace or package names return an error.

## Optional nextest validation

Pass `--validate-nextest` to `affected` or path-based `explain` to check emitted module filters with `cargo nextest list` and widen filters that fail or match no tests:

```bash
cargo litmus affected --merge-base origin/main --validate-nextest --format json
```

`cargo nextest list` compiles test targets and executes build scripts and proc macros; enable `--validate-nextest` only on trusted code. Without it, Litmus selects commands statically without running Cargo builds or build scripts.

## Development

```bash
cargo check --locked --all-targets
cargo nextest run --locked --profile ci
cargo clippy --all-targets -- -D warnings
cargo +nightly-2025-07-08 fmt --check
cargo deny check
cargo audit
```

### Scenario tests

`tests/integration/scenarios.rs` tests end-to-end selection accuracy against synthesized multi-workspace Git monorepos and a deterministic `cargo-ferris-wheel` shim (no installed `cargo-ferris-wheel` required). Each scenario declares two-sided package expectations: missing a `required` package fails as a false negative, and selecting beyond `allowed` packages fails as unintended widening. Add a scenario whenever a change class can narrow or widen selection.

## License

Licensed under the [MIT License](LICENSE).
