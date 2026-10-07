//! The sealing tests: a managed root is unreachable except through the director.
//!
//! Each `#[test]` runs in a fresh process (see `common`), so the scenarios here may install
//! different process-wide state. Add a scenario as a module below.
#![cfg(windows)]

#[macro_use]
#[path = "../common/mod.rs"]
mod common;

#[path = "../fakedirector/mod.rs"]
mod fakedirector;
#[path = "../ntapi/mod.rs"]
mod ntapi;

mod delete_on_close;
mod handle_ops_out_of_root_sealed;
mod link_into_root_sealed;
mod nt_delete_file_sealed;
mod odd_length_name_sealed;
mod rename_into_root_sealed;
mod shim_whiteout_not_phantom;
mod synthetic_close_reclaims;
mod write_seal;
mod write_seal_no_overlay;
