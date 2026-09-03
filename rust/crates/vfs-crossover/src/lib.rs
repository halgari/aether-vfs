//! CrossOver as a Wine host on macOS.
//!
//! The macOS peer of [`vfs_proton`]. Both answer one question — *how do I run
//! this PE under Wine, in this prefix?* — and they answer it differently
//! enough that they are separate crates rather than one with a flag:
//!
//! | | GE-Proton (Linux) | CrossOver (macOS) |
//! |---|---|---|
//! | where the runtime comes from | downloaded and extracted by `vfs-proton` | installed by the user as `CrossOver.app` |
//! | which build is acceptable | GE only — stock Proton is a silent downgrade | any CrossOver new enough to have a PE `x86_64-windows` tree |
//! | how a prefix is created | `wine wineboot -u` with `WINEPREFIX` | `cxbottle --create` |
//! | how a prefix is selected | `WINEPREFIX` in the environment | `--bottle <path>` on the command line — **`WINEPREFIX` is ignored** |
//! | what runs the PE | `<runtime>/files/bin/wine` | `CrossOver.app/…/bin/wine`, a Perl wrapper |
//!
//! What is *not* different is everything that matters to the shim:
//! [`vfs_proton::WineLaunch`] describes the launch, [`vfs_proton::command_line`]
//! spells the injector's positional argv, [`vfs_proton::vfs_env_block`] spells
//! the transport handshake, and [`vfs_proton::check_geometry`] refuses a ring
//! the child would under-map. All four are reused verbatim. This crate is the
//! Wine-selection layer and nothing else.
//!
//! # Why `--bottle` and not `WINEPREFIX`
//!
//! CrossOver's `bin/wine` is a Perl wrapper that resolves a *bottle* before it
//! honours anything else; `WINEPREFIX` alone fails with "Unable to find the
//! 'default' bottle". Measured against CrossOver 26.3. But `CXBottle::find_bottle`
//! returns a private bottle name unchanged when it starts with `/`, so an
//! **absolute path is a bottle**, and a session prefix can live wherever the
//! host put it — which is what keeps [`vfs_proton::Prefix`] usable here, path
//! helpers and all.
//!
//! # Rosetta
//!
//! On Apple Silicon the whole Wine stack and the PE inside it are x86-64,
//! translated by Rosetta 2. That is load-bearing for this project and was the
//! open question before any of this was written: the shim installs `retour`
//! inline detours by patching function prologues in ntdll, and self-modifying
//! code under a binary translator is exactly the case that can quietly not
//! work. Measured 2026-09-02 on an M4 Pro under CrossOver 26.3: hooks install,
//! the ring attaches, and a 200,000-byte read comes back correct both inline
//! and through the bulk arena. It works.

#![deny(unsafe_code)]

pub mod launch;
pub mod prefix;
pub mod runtime;

pub use launch::{command_line, launch_env, run, wine_binary};
pub use prefix::{ensure, PrefixError};
pub use runtime::{installed, verify, Runtime, VerifyError};
