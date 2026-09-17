//! Integration test binary.
//!
//! Cargo compiles every top-level file in `tests/` into its own executable, so
//! the shared harness and both suites live under this single root: the harness
//! is compiled once, and an item used by any suite counts as used instead of
//! being flagged per binary.

mod index_cli;
mod scenarios;
mod support;
