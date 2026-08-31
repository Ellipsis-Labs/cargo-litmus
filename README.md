[![Docs](https://docs.rs/cargo-litmus/badge.svg)](https://docs.rs/cargo-litmus) [![Crates.io](https://img.shields.io/crates/v/cargo-litmus)](https://crates.io/crates/cargo-litmus) [![Build & test](https://github.com/Ellipsis-Labs/cargo-litmus/actions/workflows/build.yaml/badge.svg)](https://github.com/Ellipsis-Labs/cargo-litmus/actions/workflows/build.yaml)

# cargo-litmus

![A glowing test train routed through the affected path in a Rust dependency rail yard](assets/cargo-litmus-hero.png)

Conservatively select the Rust tests affected by a Git change.

`cargo-litmus` indexes Rust source relationships, combines them with Cargo package and workspace dependencies from [`cargo-ferris-wheel`](https://github.com/Ellipsis-Labs/cargo-ferris-wheel), and emits focused `cargo nextest` commands. When it cannot prove that a narrow selection is safe, it widens to package or workspace tests instead of risking a false negative.

## Install

`cargo-litmus` requires `cargo-ferris-wheel` for affected-package discovery. [`cargo-nextest`](https://nexte.st/) is optional unless you use `--validate-nextest` or execute the generated commands.

```bash
# Fastest when cargo-binstall is available.
cargo binstall cargo-litmus cargo-ferris-wheel cargo-nextest

# Or build the two required Cargo subcommands from source.
cargo install cargo-litmus cargo-ferris-wheel
```

The minimum supported Rust version is 1.88.0.

## Quick start

From the root of a Git repository containing one or more Cargo workspaces:

```bash
# Build or update target/cargo-litmus.rkyv.
cargo litmus index --root .

# Select changes between the merge-base of origin/main and HEAD.
cargo litmus affected --root . --merge-base origin/main

# Select current staged, unstaged, deleted, renamed, and untracked changes.
cargo litmus affected --root . --worktree

# Explain why a file or package selects tests.
cargo litmus explain crates/api/src/lib.rs --root .
cargo litmus explain --package api-server --root .
```

The summary output includes selection and cache telemetry. Use `--format json` for the complete machine-readable report, or `--json-output <path>` to write that report while retaining summary telemetry on stdout.

## Selecting changes

The `affected` command accepts explicit paths as well as Git-native inputs:

```bash
# Explicit paths.
cargo litmus affected --files crates/api/src/lib.rs crates/types/src/lib.rs

# An exact revision range, including changed line ranges.
cargo litmus affected --base origin/main --head HEAD

# Only changes staged in the Git index.
cargo litmus affected --staged

# Newline-delimited paths, a JSON array, or a JSON object on stdin.
git diff --name-only origin/main...HEAD | cargo litmus affected --files-stdin
```

`--merge-base`, `--worktree`, and `--staged` are mutually exclusive. Explicit `--files` can be combined with `--base` and `--head` to restrict revision analysis to those paths.

## Index caches

The current index defaults to `$CARGO_TARGET_DIR/cargo-litmus.rkyv`, or `target/cargo-litmus.rkyv` when `CARGO_TARGET_DIR` is unset. Override it with `--cache` for `index` or `--current-cache` for `affected` and `explain`.

For revision-aware CI, build an index at the base revision and pass it to the head analysis:

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

`LITMUS_BASE_CACHE` provides the default `--base-cache` path. Cache metadata includes the repository root and source/Cargo fingerprints; incompatible or incomplete caches are rebuilt conservatively.

## Repository-specific inputs

Rust syntax and Cargo metadata cannot express every input to a build or test. Add `.cargo-litmus.toml` at the analyzed repository root to map migrations, schemas, generated inputs, or other files explicitly:

```toml
[[rules]]
paths = ["migrations/**"]
workspaces = ["server"]
selection = "workspace"

[[rules]]
paths = ["schemas/**"]
packages = ["api-types", "api-server"]
selection = "packages"

[[build-script-inputs]]
paths = ["codegen/**"]
packages = ["generated-client"]
```

Rules are strict: unknown fields, invalid globs, and empty required selections are errors. Inputs that remain unmapped widen selection rather than being ignored.

## Optional nextest validation

Pass `--validate-nextest` to `affected` or path-based `explain` to resolve the emitted filters with `cargo nextest list`. This can compile repository code and execute build scripts, so only enable it for trusted code.

```bash
cargo litmus affected --merge-base origin/main --validate-nextest --format json
```

Without validation, Litmus still emits conservative commands but does not execute Cargo builds or build scripts while selecting them.

## Development

```bash
cargo check --locked --all-targets
cargo nextest run --locked --profile ci
cargo clippy --all-targets -- -D warnings
cargo +nightly-2025-07-08 fmt --check
cargo deny check
cargo audit
```

## License

Licensed under the [MIT License](LICENSE).
