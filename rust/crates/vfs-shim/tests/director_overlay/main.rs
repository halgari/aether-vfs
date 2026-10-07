//! Content through the director: reads over both transports, the DRM names, and write failures.
//!
//! Each `#[test]` runs in a fresh process (see `common`), so the scenarios here may install
//! different process-wide state. Add a scenario as a module below.
#![cfg(windows)]

#[macro_use]
#[path = "../common/mod.rs"]
mod common;

#[path = "../fakedirector/mod.rs"]
mod fakedirector;

mod director_reads;
mod drm_names_route_to_director;
mod overlay_failure_reporting;
