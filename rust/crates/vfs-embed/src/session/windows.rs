//! The Windows serve and launch: a named-section ring, the shim injected by
//! `vfs-inject`, and the child's ring coordinates inherited through the
//! process environment.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vfs_director::ipc::IpcServe;

use super::stage::ResolvedImage;
use super::{check_image, LaunchOpts, Session};

/// Serializes **every** process-global env mutation this crate performs —
/// [`Session::serve`]'s as well as [`Session::launch`]'s.
///
/// `CreateProcessW` inherits the parent's environment (null env block), so the
/// child's ring coordinates travel as process-wide `VFS_*` vars that
/// `IpcServe::apply_env_roots` and `run_target_with_shim` both write. Two
/// sessions interleaving there hand a child the other one's ring.
///
/// The lock is not only about interleaving. `std::env::set_var` is **unsound
/// in a multi-threaded process** — it mutates a global the C runtime may be
/// reading concurrently, which is why Rust 2024 marks it `unsafe` — and the
/// hosts this crate exists for are multi-threaded by construction: a language
/// runtime's thread pool, a GUI main process. Serializing our own writers is
/// the floor, not the fix; the fix
/// is to stop touching process env at all and hand `CreateProcessW` an
/// explicit environment block built for the child (see [`Session::launch`]).
///
/// Windows-only: `serve`'s `apply_env_roots` and `launch`'s `opts.env`
/// save/set/restore. The unix bodies never write process env: a Wine child's
/// environment block is built by `vfs_proton::launch::launch_env`, `opts.env`
/// included.
#[cfg(windows)]
static LAUNCH_ENV_LOCK: Mutex<()> = Mutex::new(());

/// How long the injector waits for the shim to report ready when neither
/// [`LaunchOpts::ready_timeout`] nor `VFS_READY_TIMEOUT_SECS` says: the default
/// `vfs-injector.exe` itself uses.
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(vfs_env::DEFAULT_READY_TIMEOUT_SECS);

impl Session {
    /// The declared roots beyond root 0, as `apply_env_roots` wants them.
    ///
    /// No `id != 0` filter: [`Session::declare_root`] routes id 0 to
    /// `virtual_root`, which `apply_env_roots` is handed separately, so
    /// nothing here can be root 0; filtering id 0 here would silently discard a
    /// `declare_root(0, …)`, so the invariant is enforced in one place instead
    /// of compensated for.
    ///
    /// Windows-only: its one caller is `serve`'s `apply_env_roots`, which is
    /// itself `#[cfg(windows)]` — it publishes the named-section handshake
    /// (`VFS_RING_SECTION` plus the two event names). The file-backed ring has
    /// its own env protocol, published into the **child's** environment by
    /// `vfs_proton::launch::launch_env` rather than into this process's, and
    /// the unix `launch` hands the child its roots' **locations** through
    /// `WineLaunch::virtual_roots` instead.
    #[cfg(windows)]
    fn extra_roots_env(&self) -> Vec<(u32, String)> {
        debug_assert!(
            !self.extra_roots.iter().any(|(id, _)| *id == 0),
            "root 0 belongs in virtual_root, not extra_roots"
        );
        self.extra_roots
            .iter()
            .map(|(id, p)| (*id, p.to_string_lossy().into_owned()))
            .collect()
    }

    /// Start the control ring + workers so an injected child can remap I/O.
    /// Idempotent if already serving.
    ///
    /// This body is Windows-only because of *what it does*, not because the
    /// ring is missing elsewhere: it creates a named section, an event pair and
    /// a thin config, and publishes `VFS_RING_SECTION` — the named-section half
    /// of `vfs_director::ipc`. The unix body below starts the file-backed ring
    /// instead. The signature is identical on both targets so a host compiles
    /// unchanged.
    #[cfg(windows)]
    pub fn serve(&mut self) -> Result<(), String> {
        if !self.begin_serve()? {
            return Ok(());
        }

        let section = format!(
            "Local\\vfs_ring_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        let ipc =
            IpcServe::start_with_workers(Arc::clone(&self.kernel), section, self.io_workers())?;
        let root_s = self.virtual_root.to_string_lossy().into_owned();
        let thin = self.state_dir.join("fuse.cfg");
        ipc.write_thin_config(&thin, &root_s)?;
        // Under the same lock `launch` holds. `apply_env_roots` writes ten
        // process-global `VFS_*` vars; doing that outside the lock let a
        // second session's `serve` land between this one's `serve` and its
        // `launch` and repoint the ring the child would inherit — and, in a
        // multi-threaded host, race any other thread reading the environment.
        // See [`LAUNCH_ENV_LOCK`].
        {
            let _guard = LAUNCH_ENV_LOCK
                .lock()
                .map_err(|_| "launch env lock poisoned".to_string())?;
            ipc.apply_env_roots(
                &root_s,
                &self.extra_roots_env(),
                &thin,
                self.registry_attached(),
            );
        }

        // No `shim.cfg` here: `launch` writes it from the root as it is then.

        self.ipc = Some(ipc);
        Ok(())
    }

    /// Launch `opts.image` under the virtual root with dual-layer inject.
    /// Child sees remapped I/O for paths under `virtual_root`.
    ///
    /// Requires [`serve`] first. On `wait: false`, keep this `Session` alive.
    ///
    /// ## How `image` is resolved
    ///
    /// By [`crate::image::classify_image`] against [`Session::root_locations`]
    /// (see [`LaunchOpts::image`]), in one of three forms:
    ///
    /// - **Relative**: a vpath in root 0. The real file under the virtual
    ///   root is launched if there is one (a host whose root is a real game
    ///   directory — fixtures, a vanilla install); otherwise, if root 0's
    ///   **provider graph** serves the vpath, it is staged — see below.
    /// - **Absolute, inside a root's location**: that root's vpath, resolved
    ///   the same way — the real file in the root's directory, else staged
    ///   (root 0 only; another root's graph-only image is refused by name).
    /// - **Absolute, outside every root**: launched as given.
    ///
    /// An in-root image that is neither a real file nor served is refused by
    /// name, because `CreateProcess` would only fail later and less clearly.
    ///
    /// ## Launching an image that is VFS content
    ///
    /// `CreateProcess` reads the image off the filesystem before any hook of
    /// ours is installed in the child, and the Windows loader resolves its
    /// static imports in the same window. So an exe that only the provider
    /// graph holds — a game served out of archives into a deliberately empty
    /// managed root — is written to disk first, with its PE import closure,
    /// and the staging directory is mounted back into the graph *underneath*
    /// the curated content so the same bytes stay answerable at their vpath.
    /// [`Session::stage_launch`] is that sequence and this method calls it;
    /// [`LaunchOpts::stage_also`] and [`LaunchOpts::stage_fallback_dirs`] are
    /// its two knobs.
    ///
    /// The staged directory is held by the session (`CreateProcess` keeps the
    /// image mapped for the child's lifetime), so a detached launch
    /// (`wait: false`) requires the session to outlive the child — which it
    /// already did for the ring.
    ///
    /// ## Process-global environment
    ///
    /// The child receives its ring coordinates by **inheriting** them:
    /// `CreateProcessW` is called with a null environment block, so this
    /// method and [`Session::serve`] set process-wide `VFS_*` variables and
    /// `opts.env` entries, then restore them. [`LAUNCH_ENV_LOCK`] serializes
    /// that, which is enough for two sessions in one host and **not** enough
    /// for a host with unrelated threads: `std::env::set_var` races anything
    /// else reading the environment, and a language binding always has such
    /// threads.
    ///
    /// Removing the hazard means never writing process env: build the child's
    /// environment block explicitly and pass it to `CreateProcessW`. That is a
    /// change to `vfs_inject::RunConfig` (which owns the `CreateProcessW`
    /// call) plus a caller-supplied set of variables instead of
    /// `IpcServe::apply_env_roots`'s global writes; the shim side reads the
    /// same names out of the child's own environment either way, so it does
    /// not move. Worth doing before a second host depends on the current
    /// shape.
    #[cfg(windows)]
    pub fn launch(&self, opts: &LaunchOpts) -> Result<i32, String> {
        let ipc = self.require_serving()?;
        check_image(opts)?;

        // Root 0 may have been declared after `serve`, which created the
        // managed root it had then; resolving (and staging) needs this one.
        std::fs::create_dir_all(&self.virtual_root)
            .map_err(|e| format!("launch: create root {}: {e}", self.virtual_root.display()))?;
        let target = match self.resolve_launch_image(opts)? {
            ResolvedImage::InRoot { host, .. } => host,
            ResolvedImage::Outside(p) => PathBuf::from(p),
        };
        // `serve` wrote the thin config from root 0's location as it was then;
        // root 0 may have been declared since. Write `shim.cfg` and rewrite the
        // thin config from the current one so the shim is told the root this
        // child sees.
        let root_s = self.virtual_root.to_string_lossy().into_owned();
        let config_path = self.write_shim_config(&root_s)?;
        let thin = self.state_dir.join("fuse.cfg");
        ipc.write_thin_config(&thin, &root_s)?;
        let ready_path = self.fresh_ready_flag();

        let (dll, payload) = locate_shim_payload(opts)?;
        // Remote LoadLibrary resolves relative to the *child* cwd (managed root,
        // which is intentionally empty). Always use absolute DLL paths.
        // Strip the `\\?\` verbatim prefix — some LoadLibrary paths reject it.
        let strip_verbatim =
            |s: String| s.strip_prefix(r"\\?\").map(|t| t.to_string()).unwrap_or(s);
        let dll = strip_verbatim(
            std::fs::canonicalize(&dll)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(dll),
        );
        let payload = strip_verbatim(
            std::fs::canonicalize(&payload)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(payload),
        );
        let config_path_s = strip_verbatim(
            std::fs::canonicalize(&config_path)
                .unwrap_or(config_path.clone())
                .to_string_lossy()
                .into_owned(),
        );
        let ready_path_s = ready_path.to_string_lossy().into_owned();

        // Serialize env mutation: ring env + per-child fixture vars inherit via
        // CreateProcessW(null environment).
        let _guard = LAUNCH_ENV_LOCK
            .lock()
            .map_err(|_| "launch env lock poisoned".to_string())?;

        // Re-published here, not only in `serve`: the launch env lock is held
        // from this point, and a root declared after `serve` (or by another
        // session sharing this process's environment) must reach this child.
        ipc.apply_env_roots(
            &root_s,
            &self.extra_roots_env(),
            &thin,
            self.registry_attached(),
        );

        let mut saved: Vec<(String, Option<String>)> = Vec::with_capacity(opts.env.len() + 2);
        // `run_target_with_shim` publishes these for the target to inherit;
        // they must not outlive this launch in a host that launches again.
        // Saved before `opts.env` and before the ready timeout is read below.
        for k in [vfs_env::READY_TIMEOUT_SECS, vfs_env::CHILD_REFUSED_LOG] {
            saved.push((k.to_string(), std::env::var(k).ok()));
        }
        for (k, v) in &opts.env {
            saved.push((k.clone(), std::env::var(k).ok()));
            std::env::set_var(k, v);
        }

        let ready_timeout = opts.ready_timeout.unwrap_or_else(|| {
            vfs_env::text(vfs_env::READY_TIMEOUT_SECS)
                .and_then(|s| s.parse().ok())
                .map(Duration::from_secs)
                .unwrap_or(DEFAULT_READY_TIMEOUT)
        });

        let exit = vfs_inject::run_target_with_shim(vfs_inject::RunConfig {
            target_exe: target.to_string_lossy().into_owned(),
            args: opts.args.clone(),
            current_dir: Some(root_s),
            dll_path: dll,
            config_path: config_path_s,
            ready_path: ready_path_s.clone(),
            ready_timeout,
            payload_path: payload,
            preinit_redirects: vec![],
            detach: !opts.wait,
        });

        for (k, old) in saved {
            match old {
                Some(v) => std::env::set_var(&k, v),
                None => std::env::remove_var(&k),
            }
        }

        // Children the shim killed because it could not inject them, one
        // `<image> <reason>` line each, beside the ready file.
        let refused =
            std::fs::read_to_string(format!("{ready_path_s}{}", vfs_env::CHILD_REFUSED_SUFFIX))
                .unwrap_or_default();
        exit.map_err(|e| {
            let mut msg = format!("launch: {e:?}");
            for line in refused.lines().filter(|l| !l.trim().is_empty()) {
                msg.push_str(&format!(
                    "\na child process was refused and killed because the shim could not \
                     inject it (`<image> <reason>`): {}",
                    line.trim()
                ));
            }
            msg
        })
    }
}

// Only `launch`'s Windows body calls this (it resolves `vfs_inject`'s DLL/
// payload pair), so it is gated alongside it.
#[cfg(windows)]
fn locate_shim_payload(opts: &LaunchOpts) -> Result<(String, String), String> {
    let text = |p: &std::path::Path| p.to_string_lossy().into_owned();
    if let (Some(d), Some(p)) = (&opts.shim_dll, &opts.payload_dll) {
        return Ok((text(d), text(p)));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dll = opts
        .shim_dll
        .as_deref()
        .map(text)
        .or_else(|| {
            vfs_inject::find_near(&exe, "vfs_shim_dll.dll")
                .map(|p| p.to_string_lossy().into_owned())
        })
        .ok_or_else(|| "vfs_shim_dll.dll not found (set LaunchOpts.shim_dll)".to_string())?;
    let payload = opts
        .payload_dll
        .as_deref()
        .map(text)
        .or_else(|| vfs_inject::ensure_payload_beside_shim(&dll, None))
        .ok_or_else(|| "vfs_payload.dll not found".to_string())?;
    Ok((dll, payload))
}
