#![deny(unsafe_code)]

//! `vfs-shim`: installs NT detours that answer every operation on a path under a
//! managed root from the director, over its ring. Supports a full in-process
//! install and the dual-layer `install_late` (the early payload owns the four
//! path/attr stubs). There is no shim-local answer: without the director's
//! client attached nothing is under a root.

#[macro_use]
mod detour_table;
mod bootstrap;
/// Lock-free record of the hook currently executing, in a shared file.
pub mod breadcrumb;
pub mod director;
mod child;
mod handle_tags;
mod hook;
mod hookstats;
mod lazy_section;
mod ntbuf;
mod ntdef;
mod read_cache;
/// The registry hooks' client for the director's registry overlay.
pub mod regclient;
mod regkeys;
mod regnotify;
mod regquery;
mod regwrite;
mod sync;
mod synth_file;
mod synth_section;
mod tramp;

pub use bootstrap::{
    BootstrapError, bootstrap_from_config_path, bootstrap_from_config_path_with_payload,
    load_static_imports_from_config_path, static_imports_to_preinit, sync_bootstrap,
};
// The encoders (and `StaticImport`) live in `vfs_protocol::shimcfg` — pure
// byte assembly with no Windows dependency — so a native Linux Director can
// build a shim config too. Re-exported here so every existing caller
// (`vfs-embed`, `vfs-inject`'s tests, `vfs-shim/tests/diagnostics/exit_stall_repro.rs`)
// keeps compiling unchanged against `vfs_shim::`.
#[doc(hidden)]
pub use hook::as_shim_io_for_tests;
/// Run one `extern "system"` entry point's body with its panic contained.
///
/// Exported for `vfs-shim-dll`, which owns the injected DLL's two other
/// `extern "system"` entry points (`DllMain` and `vfs_shim_sync_bootstrap`). An
/// unwind out of any `extern` frame is an immediate `abort()` of the *game*
/// process, so "every entry point of the shim contains its panic" is a property
/// of the DLL rather than of this crate — and it is checked as one, across both
/// crates' sources, by
/// `no_extern_hook_bypasses_the_panic_containment_macro`.
pub use hook::contain_panic;
pub use hook::{
    HookGuard, InstallError, install, install_late, registry_detours_installed, skipped_detours,
    tracked_handle_count,
};
/// The under-root open classifier's counters. Exported so a gate's own tests
/// can assert that a bypass class it closed reads **zero** — see
/// [`hookstats::outcome_count`]. A class nobody asserts on is a class that can
/// quietly start (or stop) counting again.
pub use hookstats::{
    OpenOutcome, RegNotify, delete_on_close_refused_count, hook_panic_count, hook_panics_total,
    link_refused_count, outcome_count, reg_notify_count, reg_overlay_disabled_by,
    reg_read_fallback_count, reg_unresolved_count, reg_write_refused_count,
    unrouted_director_opens,
};
pub use vfs_protocol::shimcfg::{
    StaticImport, encode_config, encode_config_full,
};

/// The canonical path the registry hooks recorded for a key handle (synthetic or
/// pass-through), if they track it. For tests and diagnostics.
pub fn registry_handle_path(handle: isize) -> Option<String> {
    regkeys::path_of(handle)
}

/// Live key handles the registry hooks track: (synthetic, pass-through).
pub fn registry_handle_counts() -> (usize, usize) {
    regkeys::counts()
}

/// Key handles with a kept enumeration snapshot (registry query hooks). For tests and
/// diagnostics.
pub fn registry_enum_states() -> usize {
    regquery::states()
}

/// Handles the registry hooks remember as not keys they serve. For tests and diagnostics.
pub fn registry_not_ours_count() -> usize {
    regkeys::not_ours_count()
}

/// Registry change notifications waiting on the overlay. For tests and diagnostics.
pub fn registry_notify_pending() -> usize {
    regnotify::pending()
}

/// Whether a handle value is one of the registry hooks' synthetic key handles.
pub fn is_synthetic_key_handle(handle: isize) -> bool {
    regkeys::is_synthetic(handle)
}
pub use vfs_inject::PayloadConfig;
