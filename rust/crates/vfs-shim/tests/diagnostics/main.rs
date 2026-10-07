//! The breadcrumb, install coverage and exit-stall diagnostics.
//!
//! Each `#[test]` runs in a fresh process (see `common`), so the scenarios here may install
//! different process-wide state. Add a scenario as a module below.
#![cfg(windows)]

#[macro_use]
#[path = "../common/mod.rs"]
mod common;

#[path = "../fakedirector/mod.rs"]
mod fakedirector;

mod breadcrumb;
mod child_inject_fails_closed;
mod exit_stall_repro;
mod hook_coverage;
