//! The ntdll detours. ALL `unsafe` in the crate lives here.
#![allow(unsafe_code)]

mod close;
mod dirquery;
mod entry;
mod file_attr;
mod file_info;
mod file_io;
mod file_mutate;
mod file_open;
mod handles;
mod install;
mod path;
mod process;
mod registry;
mod section;
#[cfg(test)]
mod test_support;

pub(crate) use self::entry::ShimIoGuard;
pub use self::entry::{as_shim_io_for_tests, contain_panic};
pub use self::install::{
    HookGuard, InstallError, install, install_late, registry_detours_installed, skipped_detours,
};
pub(crate) use self::registry::reg_real;

// Every module reaches the others' `pub(super)` items through here.
use self::close::*;
use self::dirquery::*;
use self::entry::*;
use self::file_attr::*;
use self::file_info::*;
use self::file_io::*;
use self::file_mutate::*;
use self::file_open::*;
use self::handles::*;
use self::install::*;
use self::path::*;
use self::process::*;
use self::registry::*;
use self::section::*;

use crate::engine::Engine;
use std::sync::OnceLock;

/// Opt-in only: when `VFS_ALLOW_DISK_FALLTHROUGH=1`, under-root FUSE NOT_FOUND
/// may open the host path (legacy / debug). Default **off** — game content must
/// come from the director (zip/overrides), never the Steam library tree.
fn allow_disk_fallthrough() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| vfs_env::opt_in(vfs_env::ALLOW_DISK_FALLTHROUGH))
}

/// Whether a child process we inject starts with its working directory set to
/// the virtual root. Default **on**; `VFS_CHILD_CWD_ROOT=0` disables.
///
/// A launcher sets the child's cwd to its own directory — SKSE points it at the
/// staged launch dir. Two things then break: `SteamAPI_Init` reads
/// `steam_appid.txt` from the *cwd* and fails DRM with "Application load error
/// 3:0000065432" (a modal dialog, so the child hangs rather than exits), and the
/// game resolves `Data/` from there and finds no content. The virtual root is
/// where both actually live.
fn child_cwd_root() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| vfs_env::opt_out(vfs_env::CHILD_CWD_ROOT))
}

static ENGINE: OnceLock<Engine> = OnceLock::new();
