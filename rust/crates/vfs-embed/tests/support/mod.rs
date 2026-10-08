//! Shared support for vfs-embed's end-to-end tests. Each test file pulls it in
//! with `mod support;`, so every file compiles its own copy and uses a subset
//! of it — hence the blanket `dead_code` allowance.
//!
//! This is a `tests/support/mod.rs`, not a top-level `tests/*.rs` file, so
//! cargo does not compile it as its own (empty) test binary.
//!
//! - **Unix: the Proton launch** ([`proton`], re-exported here): scratch,
//!   artefact lookup and prerequisite policy for the tests that run a Windows
//!   program under GE-Proton. See that module's own doc for its three rules.
//! - **Windows: the injected launch** ([`artifacts`], [`escape`], [`launch`]):
//!   building and locating the shim and fixtures beside the test binary,
//!   running `vfs-fixture-escape`, and serialising and bounding launches.
//!   They compile everywhere, so a test file's helpers do too; only the tests
//!   that call them are `#[cfg(windows)]`.
//! - **Either: a composed, served session** ([`session`]) and the shim's
//!   hook-stats report ([`shim_report`]).

#![allow(dead_code, unused_imports)]

#[cfg(unix)]
mod proton;
#[cfg(unix)]
pub use proton::*;

pub mod artifacts;
pub mod escape;
pub mod launch;
pub mod session;
pub mod shim_report;

pub use shim_report::*;
pub use vfs_testkit::zip::write_stored_zip;
