#![deny(unsafe_code)]

//! `vfs-shim`: installs NT detours that redirect virtualized paths to mod
//! backing files. Supports standalone in-process install and dual-layer
//! install_late (early payload owns the four path/attr stubs).

mod bootstrap;
/// Lock-free record of the hook currently executing, in a shared file.
pub mod breadcrumb;
mod engine;
pub mod fuse_client;
mod fuse_synth;
mod hook;
mod hookstats;
mod inject;
mod lazy_section;
mod ntbuf;
mod ntdef;
mod overlay;
mod read_cache;
/// The registry hooks' client for the director's registry overlay.
pub mod regclient;
mod regkeys;
mod regnotify;
mod regquery;
mod regwrite;
mod zipserve;

pub use bootstrap::{
    bootstrap_from_config_path, bootstrap_from_config_path_with_payload, decode_config,
    decode_config_full, load_static_imports_from_config_path, static_imports_to_preinit,
    sync_bootstrap, BootstrapError,
};
// The encoders (and `StaticImport`) live in `vfs_protocol::shimcfg` — pure
// byte assembly with no Windows dependency — so a native Linux Director can
// build a shim config too. Re-exported here so every existing caller
// (`vfs-embed`, `vfs-inject`'s tests, `vfs-shim/tests/exit_stall_repro.rs`)
// keeps compiling unchanged against `vfs_shim::`.
pub use engine::{Engine, EngineError, RenameOutcome};
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
    install, install_late, registry_detours_installed, skipped_detours, HookGuard, InstallError,
};
/// The under-root open classifier's counters. Exported so a gate's own tests
/// can assert that a bypass class it closed reads **zero** — see
/// [`hookstats::outcome_count`]. A class nobody asserts on is a class that can
/// quietly start (or stop) counting again.
pub use hookstats::{
    hook_panic_count, hook_panics_total, outcome_count, overlay_fail_count, reg_notify_count,
    reg_overlay_disabled_by, reg_read_fallback_count, reg_unresolved_count,
    reg_write_refused_count, unrouted_director_opens, OpenOutcome, OverlayFail, RegNotify,
};
pub use vfs_protocol::shimcfg::{
    encode_config, encode_config_full, encode_config_with_overlay, StaticImport,
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
pub use overlay::overlay_layer_dir;
pub use vfs_inject::PayloadConfig;
