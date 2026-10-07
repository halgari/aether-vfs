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

// Acquisition — the network query and the download/verify/extract path — is
// behind the `acquire` feature (default on). See the manifest for why: it
// carries `ureq` -> `rustls` -> `ring`, a C cross-compile in a build script,
// and `vfs-embed` consumes this crate on unix to *launch*, not to install.
pub mod artifacts;
#[cfg(feature = "acquire")]
pub mod install;
pub mod launch;
pub mod layout;
pub mod nvapi;
pub mod prefix;
mod process;
#[cfg(feature = "acquire")]
pub mod release;
pub mod runtime;
pub mod steam;
#[cfg(test)]
mod test_tmp;

// The module paths (`launch::`, `prefix::`, `nvapi::`, ...) are canonical. The
// flat re-exports below are kept for callers that already use them; the ones
// nothing in this repository or Haskill imports are hidden from the docs.
#[cfg(feature = "acquire")]
#[doc(hidden)]
pub use install::{InstallError, Installed};
#[cfg(feature = "acquire")]
pub use install::{extract_tar_gz, install_release, parse_sha512sum, partial_path, verify_digest};
#[doc(hidden)]
pub use launch::{
    BASE_DLL_OVERRIDES, LaunchError, check_extra_env, command_line, describe_injector_error,
    injector_error_path, is_reserved_env, wine_binary,
};
pub use launch::{DEFAULT_WINEDEBUG, WineLaunch, finish, launch_env, merge_dll_overrides, spawn};
pub use layout::Root;
#[doc(hidden)]
pub use prefix::PrefixError;
pub use prefix::{Prefix, PrefixInit, ensure, ensure_with};
#[cfg(feature = "acquire")]
pub use release::{Release, fetch_releases, pick};
#[cfg(feature = "acquire")]
#[doc(hidden)]
pub use release::{ResolveError, parse_releases};
#[doc(hidden)]
pub use runtime::{VerifyError, runtime_lib_env, runtime_lib_env_host};
pub use runtime::{cmp_tags, installed, installed_dirs, verify_ge};
pub use steam::{HelperStatus, SteamLaunch, SteamSide};
#[doc(hidden)]
pub use steam::{STEAM_HELPER, STEAM_HELPER_OVERRIDE};
