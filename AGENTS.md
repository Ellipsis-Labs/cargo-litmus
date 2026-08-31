# Repository Guidelines

## Project Structure & Module Organization

`cargo-litmus` is a Rust Cargo subcommand for conservative affected-test selection. The CLI entrypoint is `src/main.rs`, with clap parsing and output in `src/cli.rs`. Index construction and persistence live in `src/indexer.rs`, `src/parser.rs`, `src/model.rs`, and `src/storage.rs`. Selection logic is in `src/affected.rs`; Git change discovery, Cargo graph inspection, external `cargo-ferris-wheel` integration, optional nextest validation, and repository rules are split across their correspondingly named modules. Integration coverage lives in `tests/index_cli.rs`, and the default index is written to `target/cargo-litmus.rkyv`.

## Build, Test, and Development Commands

- `cargo build` / `cargo build --release`: Compile debug or optimized binaries.
- `cargo check --locked --all-targets`: Type-check all targets without linking.
- `cargo nextest run --locked --profile ci`: Run the canonical test suite.
- `cargo test --locked --doc`: Verify documentation tests.
- `cargo clippy --all-targets -- -D warnings`: Enforce lint-clean code.
- `cargo +nightly-2025-07-08 fmt --check`: Verify the repository's Rust formatting.
- `cargo deny check` and `cargo audit`: Check licenses, bans, and advisories.
- `cargo hold voyage`: Prepare and verify the shared Cargo cache before local CI-equivalent checks.

## Coding Style & Safety Invariants

Use Rust 2024 formatting and keep modules and files snake_case, types UpperCamelCase, and constants SCREAMING_SNAKE_CASE. Prefer typed errors with `thiserror` and surface CLI diagnostics with `miette`; avoid `unwrap()` outside tests. Litmus must prefer false positives over false negatives: uncertain inputs, incomplete dependency information, or failed self-checks should widen selection rather than silently skip tests. Preserve stable JSON fields and cache compatibility deliberately, including an index version bump when the serialized schema changes.

## Testing Guidelines

Keep focused unit tests beside their implementations and add end-to-end CLI/index scenarios to `tests/index_cli.rs`. Use temporary tracked Git repositories for change-discovery tests. Every selector change needs coverage for the narrow case and its conservative fallback. Tests must not require a developer's existing checkout, cache, or global Git configuration.

## Commit & Pull Request Guidelines

Use Conventional Commits such as `feat:`, `fix:`, and `chore(deps):`, with subject lines under 72 characters. PRs should explain any selection-safety or index-schema impact and list the proving checks. Update `README.md` for CLI, configuration, output, or installation changes.

## CI & Operational Tips

GitHub Actions runs checks, tests, doctests, linting, formatting, dependency policy, audits, and four-platform cross-builds. Runtime affected-test selection expects `cargo-ferris-wheel`; `--validate-nextest` additionally expects `cargo-nextest` and may compile code or execute build scripts, so use it only for trusted repositories. Minimum supported Rust is 1.88.0. Do not hand-edit `target/cargo-litmus.rkyv`; rebuild it through `cargo litmus index` or `cargo litmus affected`.
