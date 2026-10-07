//! Shared support for `vfs-directord`'s launch and session tests.
//!
//! This is a `tests/support/mod.rs`, not a top-level `tests/*.rs` file, so
//! cargo does not compile it as its own (empty) test binary. Each test binary
//! pulls in what it needs with `mod support;`, so every item is `pub` and the
//! ones a given binary does not use are not dead code.

#![allow(dead_code, unused_imports)]

pub mod artifacts;
pub mod escape;
pub mod launch;
pub mod shim_report;

pub use shim_report::*;
pub use vfs_testkit::zip::write_stored_zip;
