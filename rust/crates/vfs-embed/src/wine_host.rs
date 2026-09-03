//! Which Wine hosts this platform's launches, and where its prefixes live.
//!
//! [`Session::launch`](crate::Session::launch)'s unix body is otherwise
//! identical on Linux and macOS: same ring, same shim, same injector, same
//! `C:\vfs-session\…` layout linked into the prefix's `drive_c`. Three steps
//! differ — find the runtime, create the prefix, spawn `wine` — and they are
//! collected here so the launch body reads as one sequence instead of three
//! `cfg` forks in the middle of it.
//!
//! Selected by target, never by feature. Only one of the two bodies below is
//! ever compiled, which is the same rule `vfs-director` uses to pick a
//! transport: an embedder writes `Session::launch` and gets the right Wine for
//! the machine they are on, with nothing to configure.
//!
//! ## The two hosts
//!
//! - **Linux — GE-Proton** ([`vfs_proton`]). Acquired by this project,
//!   verified before use because `PROTONPATH` silently defaults to stock
//!   Proton, and selected with `WINEPREFIX`.
//! - **macOS — CrossOver** ([`vfs_crossover`]). Installed by the user,
//!   selected with `--bottle`. There is no equivalent of the GE check because
//!   there is nothing to be silently downgraded *to*.

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "linux")]
mod linux {
    use std::path::PathBuf;

    use vfs_proton::layout::Root;
    use vfs_proton::prefix::Prefix;
    use vfs_proton::{LaunchError, WineLaunch};

    /// A verified GE-Proton runtime directory.
    pub struct Host {
        dir: PathBuf,
    }

    /// The newest verified GE-Proton under `root`, or a refusal naming where
    /// one would have been installed.
    ///
    /// `installed_dirs`, not `installed` + `runtime_dir`: the tag comes from
    /// the tree's `version` file and the directory name from the release it
    /// was installed from, and re-joining the tag onto `runtimes()` assumes
    /// those always agree.
    pub fn resolve(root: &Root) -> Result<Host, String> {
        let dir = vfs_proton::runtime::installed_dirs(root)
            .map_err(|e| format!("launch: reading {}: {e}", root.runtimes().display()))?
            .into_iter()
            .next()
            .map(|(_tag, dir)| dir)
            .ok_or_else(|| {
                format!(
                    "launch: no verified GE-Proton runtime under {} — install one with \
                     `vfs-proton install` (VFS_HOME selects where it lands). Launching on \
                     stock Proton instead is the silent downgrade this path refuses.",
                    root.runtimes().display()
                )
            })?;
        Ok(Host { dir })
    }

    /// What travels in [`WineLaunch::runtime`], and what `PROTONPATH` becomes.
    pub fn runtime_path(host: &Host) -> PathBuf {
        host.dir.clone()
    }

    pub fn ensure_prefix(host: &Host, root: &Root, session: &str) -> Result<Prefix, String> {
        vfs_proton::prefix::ensure(root, &host.dir, session)
            .map_err(|e| format!("launch: wine prefix: {e}"))
    }

    /// A prefix at a directory the host named, rather than one derived under
    /// `root`. See [`Session::set_prefix_dir`](crate::Session::set_prefix_dir).
    ///
    /// `vfs_proton::prefix::ensure` composes the path itself from a session
    /// tag, so this is the same two steps it performs — the GE gate, then
    /// `wineboot` — against a caller-chosen directory.
    pub fn ensure_prefix_at(host: &Host, dir: &std::path::Path) -> Result<Prefix, String> {
        vfs_proton::prefix::ensure_at(&host.dir, dir)
            .map_err(|e| format!("launch: wine prefix: {e}"))
    }

    pub fn run(_host: &Host, l: &WineLaunch) -> Result<i32, LaunchError> {
        vfs_proton::launch::run(l)
    }

    /// Named for diagnostics, so a failure report says which Wine ran.
    pub fn describe(host: &Host) -> String {
        format!("GE-Proton at {}", host.dir.display())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::path::PathBuf;

    use vfs_proton::layout::Root;
    use vfs_proton::prefix::Prefix;
    use vfs_proton::{LaunchError, WineLaunch};

    pub use vfs_crossover::Runtime as Host;

    /// The installed CrossOver, or a refusal that says how to point at one.
    ///
    /// `root` is unused: CrossOver is a licensed application installed once,
    /// not a runtime this project acquires into its own home. Taken anyway so
    /// both hosts present one signature to the launch body.
    pub fn resolve(_root: &Root) -> Result<Host, String> {
        vfs_crossover::installed().map_err(|e| {
            format!(
                "launch: {e}. Install CrossOver, or set {} to the bundle if it is not at {}.",
                vfs_crossover::runtime::APP_ENV,
                vfs_crossover::runtime::DEFAULT_APP
            )
        })
    }

    /// [`WineLaunch::runtime`] is carried for diagnostics on this host and is
    /// **not** used to find `wine` — [`vfs_crossover::launch::wine_binary`]
    /// resolves that from the [`Host`] itself. Filling it with the CrossOver
    /// directory keeps the field meaning "the Wine that ran this" on both
    /// platforms rather than leaving it empty on one.
    pub fn runtime_path(host: &Host) -> PathBuf {
        host.dir.clone()
    }

    /// The session's bottle, under the same `sessions/<id>/prefix` path Linux
    /// uses. Shared layout on purpose: `link_into_prefix` symlinks the
    /// session's root, overlay and state into the prefix's `drive_c`, so the
    /// prefix must not live *inside* the state directory it links to.
    pub fn ensure_prefix(host: &Host, root: &Root, session: &str) -> Result<Prefix, String> {
        let session_dir = root
            .try_session_dir(session)
            .map_err(|e| format!("launch: session id: {e}"))?;
        ensure_prefix_at(host, &session_dir.join("prefix"))
    }

    /// A bottle at a directory the host named. See
    /// [`Session::set_prefix_dir`](crate::Session::set_prefix_dir) — on this
    /// platform a prefix can hold a logged-in Steam client, which makes the
    /// derived, hash-keyed name actively wrong.
    pub fn ensure_prefix_at(host: &Host, dir: &std::path::Path) -> Result<Prefix, String> {
        vfs_crossover::ensure(host, dir).map_err(|e| format!("launch: crossover bottle: {e}"))
    }

    pub fn run(host: &Host, l: &WineLaunch) -> Result<i32, LaunchError> {
        vfs_crossover::run(host, l)
    }

    pub fn describe(host: &Host) -> String {
        match &host.version {
            Some(v) => format!("CrossOver {v} at {}", host.dir.display()),
            None => format!("CrossOver at {}", host.dir.display()),
        }
    }
}
