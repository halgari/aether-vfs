//! The registry overlay's hooks and client, against the director's own registry code.
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
#[path = "../common/reg.rs"]
mod reg;

mod regclient;
mod regkeys;
mod regkeys_disabled;
mod regnotify;
mod regquery;
mod regwrite;
