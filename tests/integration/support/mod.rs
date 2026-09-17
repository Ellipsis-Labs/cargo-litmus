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
//!
//! The harness is split by concern:
//! - [`monorepo`]: the synthesis DSL and the temporary git repository,
//! - [`ferris`]: the scripted ferris-wheel shim and its payload model,
//! - [`report`]: the test-side mirror of the CLI's JSON report contract,
//! - [`expectations`]: two-sided expectations and violation diagnostics,
//! - [`scenario`]: the `check*` drivers running index → change → affected,
//! - [`env`]: `PATH` and binary resolution for scenario subprocesses.

pub mod env;
pub mod expectations;
pub mod ferris;
pub mod monorepo;
pub mod report;
pub mod scenario;

pub use expectations::{CommandExpectation, Expect, target};
pub use ferris::{FerrisCrate, FerrisPayload, FerrisWorkspace};
pub use monorepo::{Monorepo, pkg, ws};
pub use report::CommandMode;
pub use scenario::{
    check, check_with_ferris, check_with_wrapped_cargo, create, delete, edit, rename,
};
