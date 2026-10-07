//! The file hooks: attributes, enumeration, identity, relative names and writes.
//!
//! Each `#[test]` runs in a fresh process (see `common`), so the scenarios here may install
//! different process-wide state. Add a scenario as a module below.
#![cfg(windows)]
#![allow(unsafe_code)]

#[macro_use]
#[path = "../common/mod.rs"]
mod common;

#[path = "../fakedirector/mod.rs"]
mod fakedirector;
#[path = "../ntapi/mod.rs"]
mod ntapi;

mod hook_attrs;
mod hook_dir_write_open;
mod hook_direnum;
mod hook_enum_parity;
mod hook_handle_relative_unseen;
mod hook_identity;
mod hook_relative_paths;
mod hook_stat_agreement;
mod hook_write;
mod hook_write_second_root;
mod identity_objectname;
mod identity_objectname_untracked;
