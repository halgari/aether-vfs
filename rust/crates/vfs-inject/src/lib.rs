#![deny(unsafe_code)]

//! Launch a target process with VFS injection.
//!
//! - [`run_target_with_shim`] — dual-layer: pre-init early payload + full shim
//!   (static imports + full Engine) via spin-gate handoff.
//! - [`run_target_with_preinit`] — early payload only (static-import fixture).

use std::time::Duration;

mod artifacts;
mod cli;
mod pe;
mod inject;
mod map;
mod payload_cfg;
mod static_imports;
mod stub;

pub use pe::{
    is_system_import_dll, keep_host_steam_api, map_image_from_pe_bytes_local, pe_looks_like_image,
};

/// Static-import DLL names declared by a raw PE file image.
///
/// Callers staging a launch directory need the import list *before* any process
/// exists, so this maps the file image in memory rather than reading a live
/// process. Returns `None` when `raw` is not a usable PE.
pub fn import_dll_names_of_pe(raw: &[u8]) -> Option<Vec<String>> {
    let (img, _base, e_lfanew) = map::build_image(raw).ok()?;
    Some(map::import_dll_names(&img, e_lfanew))
}

/// Parameters for [`run_target_with_shim`].
pub struct RunConfig {
    pub target_exe: String,
    pub args: Vec<String>,
    /// Working directory for the child (typically the managed game root).
    pub current_dir: Option<String>,
    /// Full std shim DLL (`vfs_shim_dll.dll`).
    pub dll_path: String,
    pub config_path: String,
    pub ready_path: String,
    pub ready_timeout: Duration,
    /// Zero-import early payload DLL (`vfs_payload.dll`).
    pub payload_path: String,
    /// Early redirect-table entries (static-import DLLs); may be empty.
    pub preinit_redirects: Vec<PreinitRedirect>,
    /// When true, return as soon as the target is running (do not wait for exit).
    pub detach: bool,
}

/// One early-payload redirect: object names ending with `suffix` (final path
/// component) open the backing file at `backing_nt` (absolute NT path `\??\...`).
pub struct PreinitRedirect {
    pub suffix: String,
    pub backing_nt: String,
    pub backing_size: u64,
}

/// Parameters for [`run_target_with_preinit`].
pub struct PreinitConfig {
    pub target_exe: String,
    pub args: Vec<String>,
    /// Working directory for the child (app dir without the virtualized DLL).
    pub current_dir: Option<String>,
    pub payload_path: String,
    pub redirects: Vec<PreinitRedirect>,
}

/// Failure points in launch + inject + run.
#[derive(Debug)]
pub enum InjectError {
    CreateProcess,
    Alloc,
    Write,
    RemoteThread,
    Timeout,
    Wait,
    ExitCode,
    Ntdll,
    PayloadRead,
    PeParse,
    ThreadContext,
    Config,
    /// The shim's hooks came up, but a director was configured (a ring was
    /// named) and its FUSE client failed to attach — the process would have
    /// run completely un-virtualised. It is killed before ever being
    /// released past the pre-init spin gate, i.e. before a byte of game code
    /// runs. Distinct from [`Timeout`], which also covers a shim that never
    /// loaded at all (no injection happened) — that is not a FUSE failure and
    /// must not be reported as one.
    FuseInit(String),
}

pub use artifacts::{ensure_payload_beside_shim, find_near, resolve_payload_for_run};
pub use cli::{parse_injector_args, InjectorArgs};
pub use inject::{
    arm_preinit_payload, arm_preinit_payload_ex, inject_dll, load_static_imports_from_config,
    merge_preinit_redirects, run_target_with_preinit, run_target_with_shim, PreinitArm,
};
pub use static_imports::StaticImport as ConfigStaticImport;
