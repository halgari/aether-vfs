//! GE-Proton acquisition (and, later, launch) for aether-vfs.
//!
//! This crate is portable on purpose: it compiles and its tests run on both
//! Windows and Linux, even though extracting a Linux Proton tarball on
//! Windows produces nothing anyone can run. The payoff is that URL building,
//! digest parsing, version ordering, and directory-layout logic are all
//! exercised by the Windows CI job, which is the thicker of the two jobs in
//! this repo. Only the actual network download (Task 4) is Linux/manual-only.
//!
//! The other non-negotiable: `PROTONPATH` defaults to UMU-Proton (stock Valve
//! Proton) when unset or wrong, so every runtime this crate hands back must
//! be verified as GE-Proton. See [`runtime::verify_ge`].

pub mod layout;
pub mod release;
pub mod runtime;

pub use layout::Root;
pub use release::{fetch_releases, parse_releases, pick, Release, ResolveError};
pub use runtime::{cmp_tags, installed, verify_ge, VerifyError};
