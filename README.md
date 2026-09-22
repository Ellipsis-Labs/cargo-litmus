[![Docs](https://docs.rs/cargo-litmus/badge.svg)](https://docs.rs/cargo-litmus) [![Crates.io](https://img.shields.io/crates/v/cargo-litmus)](https://crates.io/crates/cargo-litmus) [![Build & test](https://github.com/Ellipsis-Labs/cargo-litmus/actions/workflows/build.yaml/badge.svg)](https://github.com/Ellipsis-Labs/cargo-litmus/actions/workflows/build.yaml)

# cargo-litmus

![A glowing test train routed through the affected path in a Rust dependency rail yard](assets/cargo-litmus-hero.png)

Conservatively select the Rust tests affected by a Git change.

`cargo-litmus` indexes Rust source relationships, combines them with Cargo package and workspace dependencies from [`cargo-ferris-wheel`](https://github.com/Ellipsis-Labs/cargo-ferris-wheel), and emits focused `cargo nextest` commands. When it cannot prove that a narrow selection is safe, it widens to package or workspace tests instead of risking a false negative.

## Install

`cargo-litmus` requires `cargo-ferris-wheel` for affected-package discovery. [`cargo-nextest`](https://nexte.st/) is optional unless you use `--validate-nextest` or execute the generated commands.

Litmus invokes the `cargo-ferris-wheel` binary directly and requires version 1.1.4 or newer. Direct invocation works from monorepo roots without a root `Cargo.toml`, including environments where `cargo` is wrapped by a build cache such as mbx.

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

## How inputs select tests

Litmus classifies every changed file, in this order:

1. **Configured inputs** (`.cargo-litmus.toml`) select the workspaces or packages they name.
2. **Cargo inputs** (`Cargo.toml`, `Cargo.lock`, `build.rs`, `.cargo/config.toml`, `rust-toolchain.toml`) select their owning package's closure, their workspace, or every workspace for configuration and toolchain files, which resolve by walking up the directory tree.
3. **Indexed sources** map to their owning package and reverse dependents. Files embedded with `include!`, `include_str!`, `include_bytes!`, or `#[path]` map to the packages that embed them.
4. **Inert inputs** select nothing and are not passed to `cargo-ferris-wheel`, whose ripple walk widens on files it cannot map. Documentation, media, fonts, legal and community files, container recipes, and CI, agent, and editor metadata cannot be compiled, executed, or read by a build unless an indexed source embeds them, which case 3 already covers. Inert classification stops at package roots: a file beneath a package root keeps its package's closure, because package code can read it through paths litmus cannot index (`CARGO_MANIFEST_DIR`, computed paths).
5. **Any other file** is conservative: a file beneath a package root selects that package and its reverse dependents; a file inside a workspace but outside its packages (lockfile, workspace manifest, workspace data) widens to that workspace; a file outside every workspace widens to all of them.

The built-in inert classes are:

| class | examples |
|---|---|
| documentation and media | `*.md`, `*.mdx`, `*.rst`, `*.adoc`, `*.png`, `*.svg`, `*.pdf`, `*.woff2` |
| JavaScript and TypeScript sources | `*.ts`, `*.tsx`, `*.js`, `*.jsx`, `*.mjs`, `*.cjs`, `*.mts`, `*.cts` |
| legal and community | `LICENSE*`, `COPYING*`, `NOTICE*`, `CHANGELOG*`, `CONTRIBUTING*`, `SECURITY*`, `CODEOWNERS` |
| CI, agent, and editor metadata | `.github/**`, `.agents/**`, `.claude/**`, `.cursor/**`, `.devcontainer/**`, `.vscode/**`, `.idea/**` |
| container recipes | `Dockerfile*`, `*.dockerfile`, `.dockerignore`, `docker-compose*.yml` |

JavaScript and TypeScript sources are classified inert because Cargo never compiles, runs, or consumes them; the toolchain that does is outside the index. Repositories whose Rust builds or tests read JavaScript or TypeScript trees map those paths back in with `[[rules]]`; configured rules win over inert classification.

Data and configuration files (`*.toml`, `*.yaml`, `*.json`, `*.sql`, `*.snap`, scripts, templates) are never inert, because a build or test can read them at runtime. Declare them with `.cargo-litmus.toml` rules, or add `selection = "ignore"` rules for repository-specific trees that cannot affect tests:

```toml
[[rules]]
paths = ["ts/**", "dev-docs/**"]
selection = "ignore"
```

Set `default-inert-paths = false` to disable the built-in classes and make every unmapped file conservative again. `--format json` reports every classification in `input_explanations`, including inert ones.

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

[[rules]]
paths = ["vendor/tooling/**"]
selection = "ignore"

[[build-script-inputs]]
paths = ["codegen/**"]
packages = ["generated-client"]
```

For `selection = "workspace"`, a workspace whose root manifest declares `[package]` uses that package's Cargo name as its workspace identifier, regardless of its member count. Virtual workspaces use their root directory name.

Patterns are `globset` globs: `*`, `**`, `?`, character classes (`[abc]`, `[!abc]`), and `{a,b}` alternates (`*.{js,jsx,ts,tsx}`) are supported. Patterns match repository-relative paths.

A path matched by several rules is a real input if any matching rule selects workspaces or packages; it is inert only when *every* matching rule ignores it. A broad `selection = "ignore"` rule therefore cannot shadow a narrower mapping rule, whatever the order in the file.

Rules are strict: unknown fields, invalid globs, and empty or contradictory selections are errors. Inputs that remain unmapped and are not classified inert widen selection rather than being ignored.

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

### Scenario tests

`tests/integration/scenarios.rs` covers selection accuracy end to end. Each scenario
synthesizes a multi-workspace Cargo monorepo in a temporary Git repository, builds a
base index, commits a change, and runs the real `cargo-litmus affected` binary against
it; the harness in `tests/integration/support/` provides the monorepo builders and
scripts `cargo-ferris-wheel` deterministically, so the suite needs no installed
ferris-wheel. The harness is split by concern: `monorepo.rs` (synthesis DSL and git
plumbing), `ferris.rs` (shim and payload model), `report.rs` (test-side mirror of the
JSON contract), `expectations.rs` (two-sided expectations and diagnostics),
`scenario.rs` (the `check*` drivers), and `env.rs` (`PATH`/binary resolution).

`tests/integration/main.rs` is the only integration-test root: Cargo compiles every
top-level file in `tests/` into its own executable, so the suites and the harness live
under one root to keep the harness compiled once and its dead-code analysis global.

Scenario expectations are two-sided: `required` packages fail the run when they are
missing (a false negative) and `allowed` packages fail the run when they are exceeded
(over-selection past the scenario's declared conservative allowance). Add a scenario
whenever a change class can widen or narrow selection.

## License

Licensed under the [MIT License](LICENSE).
