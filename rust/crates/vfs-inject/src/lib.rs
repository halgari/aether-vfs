#![deny(unsafe_code)]

//! Launch a target process with VFS injection.
//!
//! - [`run_target_with_shim`] — dual-layer: pre-init early payload + full shim
//!   (static imports + full Engine) via spin-gate handoff.
//! - [`run_target_with_preinit`] — early payload only (static-import fixture).

use std::time::Duration;

mod artifacts;
mod cli;
mod inject;
mod map;
mod payload_cfg;
mod pe;
mod static_imports;
mod steam_helper;
mod stub;

pub use payload_cfg::{PayloadConfig, RedirectEntry, MAX_REDIRECTS};
pub use pe::map_image_from_pe_bytes_local;

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
    /// The shim did not report ready within `ready_timeout` (or the early
    /// payload never reached its install sentinel). Once the process is past
    /// the payload and parked at the spin gate, the process is **killed**
    /// rather than released: a bootstrap that never says anything must not
    /// leave the target running un-virtualised.
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
    /// The shim loaded but could not bootstrap — a config from another build
    /// or a damaged one, an unreadable config, a hook that would not install —
    /// and said so in the ready file. Killed before release, like
    /// [`FuseInit`](InjectError::FuseInit).
    Bootstrap(String),
    /// The target exited — with this exit code, an `NTSTATUS` such as
    /// `0xC0000135` (a DLL it imports is missing) when the loader killed it —
    /// before the shim reported ready. Reported as soon as it is seen rather
    /// than after the ready timeout, which is what a dead target used to cost.
    TargetExited(u32),
}

pub use artifacts::{ensure_payload_beside_shim, find_near, resolve_payload_for_run};
pub use cli::{parse_injector_args, InjectorArgs};
pub use inject::{
    arm_preinit_payload, arm_preinit_payload_ex, inject_dll, load_static_imports_from_config,
    merge_preinit_redirects, run_target_with_preinit, run_target_with_shim, PreinitArm,
};
pub use static_imports::StaticImport as ConfigStaticImport;
pub use steam_helper::{
    active_process_pid, check_helper_command, clear_active_process_pid, running_under_wine,
    start_steam_helper, SteamHelper, SteamHelperError, ACTIVE_PROCESS_KEY,
};
