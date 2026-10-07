//! Copy-up through the director, the DRM names, and overlay failure reporting.
//!
//! Each `#[test]` runs in a fresh process (see `common`), so the scenarios here may install
//! different process-wide state. Add a scenario as a module below.
#![cfg(windows)]

#[macro_use]
#[path = "../common/mod.rs"]
mod common;

#[path = "../fakedirector/mod.rs"]
mod fakedirector;

mod cow_seed_reads_through_director;
mod cow_seed_reentrancy;
mod cow_seed_reporting;
mod drm_names_route_to_director;
mod drm_overlay_recursion_gone;
mod overlay_failure_reporting;
