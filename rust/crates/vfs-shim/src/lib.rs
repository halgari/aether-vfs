#![deny(unsafe_code)]

//! `vfs-shim`: installs NT detours that redirect virtualized paths to mod
//! backing files. Supports standalone in-process install and dual-layer
//! install_late (early payload owns the four path/attr stubs).

mod bootstrap;
mod engine;
pub mod fuse_client;
mod fuse_synth;
mod hook;
mod hookstats;
mod inject;
mod lazy_section;
mod ntdef;
mod overlay;
mod payload_abi;
mod zipserve;

pub use bootstrap::{
    bootstrap_from_config_path, bootstrap_from_config_path_with_payload, decode_config,
    decode_config_full, encode_config, encode_config_full, encode_config_with_overlay,
    load_static_imports_from_config_path, static_imports_to_preinit, sync_bootstrap,
    BootstrapError, StaticImport,
};
pub use engine::{Engine, EngineError, RenameOutcome};
pub use hook::{install, install_late, HookGuard, InstallError};
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
/// The under-root open classifier's counters. Exported so a gate's own tests
/// can assert that a bypass class it closed reads **zero** — see
/// [`hookstats::outcome_count`]. A class nobody asserts on is a class that can
/// quietly start (or stop) counting again.
pub use hookstats::{
    hook_panic_count, hook_panics_total, outcome_count, overlay_fail_count,
    unrouted_director_opens, OpenOutcome, OverlayFail,
};
pub use overlay::overlay_layer_dir;
pub use payload_abi::PayloadConfig;
