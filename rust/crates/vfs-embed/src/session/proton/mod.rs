//! The Proton (Wine) delivery of a session: the file-backed ring, the prefix
//! and its root links, and the launch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use vfs_director::ipc::IpcServe;
use vfs_proton::{
    launch::WineLaunch,
    layout::Root as ProtonRoot,
    prefix::{Prefix, PrefixInit},
    steam::SteamSide,
};

use super::stage::{empty_tree_snapshot, ResolvedImage};
use super::{LaunchExit, LaunchOpts, Session, STOPPED_EXIT_CODE};
use crate::image::{self, RootLocation};

mod handle;
mod ring;

pub use handle::{LaunchHandle, LaunchStopper};
pub(super) use handle::{AnonPrefix, StopInner};
use handle::StartingGuard;
pub(super) use ring::remove_memory_ring;
use ring::ring_in_memory;

impl Session {
    /// Unix: the aether-vfs home `launch` takes GE-Proton runtimes
    /// (`<home>/runtimes`) and prefixes (`<home>/sessions`) from, instead of
    /// `VFS_HOME` / `XDG_DATA_HOME` / `HOME` — so a host needs no process
    /// environment to choose it.
    #[cfg(unix)]
    pub fn set_home(&mut self, home: impl Into<PathBuf>) {
        self.home = Some(home.into());
    }

    /// The aether-vfs home `launch` uses: [`Session::set_home`]'s, else the
    /// environment's (`vfs_proton::layout::Root::from_env`).
    #[cfg(unix)]
    fn proton_home(&self) -> Result<ProtonRoot, String> {
        match &self.home {
            Some(h) => Ok(ProtonRoot::at(h.clone())),
            None => ProtonRoot::from_env()
                .map_err(|e| format!("launch: no aether-vfs home (set_home, or VFS_HOME): {e}")),
        }
    }

    /// Unix: how `launch` creates (and upgrades) the Wine prefix. The default,
    /// [`PrefixInit::Wineboot`], is `wineboot -u` into
    /// `sessions/<name>/prefix`. [`PrefixInit::Proton`] runs the runtime's
    /// own `proton` setup into `sessions/<name>/compat/pfx`, which is what a
    /// game needs (DXVK and vkd3d-proton, the DirectX and Visual C++
    /// redistributables, Steam's `lsteamclient.dll`) — see
    /// `vfs_proton::prefix::ensure_with`. The two live in different
    /// directories, so switching one named prefix between them starts a new
    /// prefix rather than converting the old one.
    #[cfg(unix)]
    pub fn set_prefix_init(&mut self, init: PrefixInit) {
        self.prefix_init = init;
    }

    /// Unix: whether a launch in a [`PrefixInit::Proton`] prefix starts
    /// Proton's Steam helper, so the program's Steam API finds the running
    /// Steam client (`SteamAPI_IsSteamRunning` is true) — see
    /// `vfs_proton::steam`. On by default. It takes effect when the launch
    /// has an app id ([`PrefixInit::Proton`]'s, else `SteamAppId` in
    /// [`LaunchOpts::env`]) and the Steam client is running; when the client
    /// is not, the launch goes ahead exactly as with this off and says so in
    /// one line ([`LaunchHandle::notes`], and at the top of
    /// [`LaunchOpts::log_file`]). Without the helper, a launch in a
    /// [`PrefixInit::Proton`] prefix still asks the injector to clear the pid
    /// an earlier helper left there, so the program's Steam API does not find
    /// a client by coincidence of Wine's pid numbering.
    /// [`LaunchHandle::steam_helper_status`] says what the injector did.
    #[cfg(unix)]
    pub fn set_steam_helper(&mut self, on: bool) {
        self.steam_helper = on;
    }

    /// Unix: the directory the Steam client keeps its runtime state in
    /// (`steam.pid`), for a client that does not use `$HOME/.steam`. Only
    /// the pid file is read, to tell whether a client is running.
    #[cfg(unix)]
    pub fn set_steam_state_dir(&mut self, dir: impl Into<PathBuf>) {
        self.steam_state_dir = Some(dir.into());
    }

    /// The Steam side of a launch with `env` as its [`LaunchOpts::env`], and
    /// the lines the launch should say about it: the helper when this
    /// session's prefix is Proton's, the launch has an app id and the Steam
    /// client is running; otherwise, in a Proton prefix, only clearing the
    /// pid an earlier helper left, with a note when the client is what is
    /// missing.
    #[cfg(unix)]
    fn steam_launch(&self, env: &BTreeMap<String, String>) -> (SteamSide, Vec<String>) {
        let PrefixInit::Proton {
            steam_client,
            app_id,
        } = &self.prefix_init
        else {
            return (SteamSide::Untouched, Vec::new());
        };
        let app_id = app_id
            .or_else(|| env.get("SteamAppId").and_then(|v| v.trim().parse().ok()))
            .filter(|id| *id != 0);
        let (true, Some(app_id)) = (self.steam_helper, app_id) else {
            return (SteamSide::Off, Vec::new());
        };
        let Some(state) = self
            .steam_state_dir
            .clone()
            .or_else(vfs_proton::steam::state_dir)
        else {
            return (
                SteamSide::Off,
                vec![
                    "aether-vfs: HOME is not set, so no Steam client can be found and the \
                      program runs without Steam"
                        .to_string(),
                ],
            );
        };
        match vfs_proton::steam::running_client(&state) {
            Some(_) => (
                SteamSide::Helper(vfs_proton::SteamLaunch {
                    client: steam_client.clone(),
                    app_id,
                }),
                Vec::new(),
            ),
            None => (
                SteamSide::Off,
                vec![vfs_proton::steam::not_running_note(&state)],
            ),
        }
    }

    /// Unix: whether `location` is one `launch` can link a root at — a `C:\…`
    /// path below the drive root, without `..`
    /// ([`vfs_proton::prefix::parse_location`], the rule `launch` itself
    /// applies). [`Session::declare_root`] stays infallible, so a host that
    /// takes locations from a user calls this first and refuses a bad one at
    /// declare time rather than at the first launch. The error names the
    /// location and what is wrong with it.
    #[cfg(unix)]
    pub fn check_root_location(location: &str) -> Result<(), String> {
        vfs_proton::prefix::parse_location(location)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Select a **persistent** Wine prefix, `$VFS_HOME/sessions/<name>/`,
    /// reused across runs and never deleted by aether-vfs. Without a name,
    /// `launch` boots an anonymous prefix keyed by `state_dir` and this
    /// session deletes it when it drops.
    ///
    /// `name` must be one plain path component (no separator, no `..`, not
    /// empty, not absolute) — the same rule as the anonymous id.
    #[cfg(unix)]
    pub fn set_prefix_name(&mut self, name: &str) -> Result<(), String> {
        ProtonRoot::at(PathBuf::new())
            .try_session_dir(name)
            .map_err(|e| format!("prefix name {name:?} must be one plain path component: {e}"))?;
        self.prefix_name = Some(name.to_string());
        Ok(())
    }

    /// Start the control ring + workers, file-backed, so a shim inside Wine
    /// can remap its I/O to this native director. Idempotent if already
    /// serving.
    ///
    /// Same shape as the Windows body above, with the transport swapped: the
    /// ring is a **real file** named `state_dir/ring.bin` that both sides
    /// `mmap` by path, because a Wine process and a native Linux director
    /// share no named section and no event either could wake the other with
    /// (see `IpcServe::start_file_backed`).
    ///
    /// **The file's pages live in memory when they can.** A ring is 64 MiB of
    /// shared mapping that both sides write constantly, and on a disk
    /// filesystem the kernel writes those pages back every writeback cycle for
    /// as long as the game runs — on btrfs with compression, compressing them
    /// first. Nothing ever reads them back from disk. So when
    /// `$XDG_RUNTIME_DIR` is a tmpfs the file is created there and
    /// `state_dir/ring.bin` is a symlink to it; Wine resolves the link like
    /// any other path, so the child opens the same name as before. Without
    /// such a directory the ring is a plain file in `state_dir`, as it always
    /// was.
    ///
    /// Two things the Windows body does are deliberately **not** done here:
    ///
    /// - **No `VFS_*` is published into this process's environment.** A Wine
    ///   child gets an explicit environment block from
    ///   `vfs_proton::launch::launch_env`, so there is nothing for it to
    ///   inherit and no window in which another session could repoint it.
    /// - **No `shim.cfg` is written.** Its contents are the managed root and
    ///   overlay *as the shim sees them*, and those `C:\` names do not exist
    ///   until a Wine prefix does — so [`Session::launch`] writes it.
    #[cfg(unix)]
    pub fn serve(&mut self) -> Result<(), String> {
        if self.ipc.is_some() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.virtual_root)
            .map_err(|e| format!("create root: {e}"))?;
        std::fs::create_dir_all(&self.overlay).map_err(|e| format!("create overlay: {e}"))?;
        std::fs::create_dir_all(&self.state_dir).map_err(|e| format!("create state: {e}"))?;

        let named = self.state_dir.join(RING_FILE);
        // Unlinked rather than reused. `FileMapping::create` grows a file but
        // never shrinks one and `ring::init` rewrites the header in place, so a
        // ring left by an earlier run of this session would be re-initialised
        // underneath anything still mapping it. Unlinking gives this director a
        // fresh inode and leaves such a reader on the old one, where it fails
        // visibly instead of racing us for slots.
        let _ = std::fs::remove_file(&named);
        let mut backing = ring_in_memory(&self.state_dir, &named);
        let start = |ring: &Path| {
            IpcServe::start_file_backed_with_workers(
                Arc::clone(&self.kernel),
                ring,
                PROTON_PAYLOAD_CAP,
                self.io_workers(),
            )
        };
        let ipc = match backing.as_deref().map(start) {
            Some(Ok(ipc)) => ipc,
            // The memory-backed ring could not be started (the tmpfs is
            // full, say). That is an optimisation failing, not the session:
            // take the link away and serve from the state directory.
            Some(Err(_)) => {
                remove_memory_ring(backing.take().as_deref(), &named);
                let _ = std::fs::remove_file(&named);
                start(&named)?
            }
            None => start(&named)?,
        };

        self.ring_backing = backing;
        self.ipc = Some(ipc);
        Ok(())
    }

    /// A stable, traversal-safe id for this session's Wine prefix, derived
    /// from its `state_dir`.
    ///
    /// `Session` has no id of its own, and the prefix needs to be the *same*
    /// directory across two launches of one session (`wineboot` is expensive
    /// and [`vfs_proton::prefix::ensure`] is idempotent only against a stable
    /// name) while being a *different* one for two live sessions, which each
    /// link their own directories into the prefix's `drive_c`. `state_dir` is
    /// exactly that identity: [`Session::new`] gives every session a unique
    /// one, and a host that points two sessions at one state directory has
    /// already handed them a single ring file to fight over.
    ///
    /// The prefix it names is **anonymous**: this session boots it at its
    /// first launch, reuses it for every later launch, and deletes it when it
    /// drops — it never outlives the session, so no later session (or later
    /// run) ever finds it again. Hence the hash may be `DefaultHasher`, whose
    /// output is stable within a build but not promised across Rust releases:
    /// the id only has to agree with itself for one session's lifetime.
    #[cfg(unix)]
    fn wine_session_id(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.state_dir.hash(&mut h);
        format!("session-{:016x}", h.finish())
    }

    /// Links the overlay and the state directory into
    /// `prefix/drive_c/vfs-session/`, and returns the two `C:\` paths they
    /// are reachable at, in that order. The roots are linked at their own
    /// locations by [`Session::link_roots`].
    ///
    /// **A Wine process can only name what is under one of its drives**, and
    /// these live wherever the host put them — normally under `/tmp`, outside
    /// the prefix entirely. Symlinks into `drive_c` rather than a `dosdevices`
    /// letter each ([`Prefix::map_drive`]): one location instead of a letter
    /// per directory, [`Prefix::windows_path`] renders the result, and every
    /// path the shim is handed is a subdirectory rather than a bare drive root.
    ///
    /// Each link is replaced, not created-if-absent: a session relaunches into
    /// the prefix it already booted, and `set_overlay`/`set_state_dir` may
    /// have moved the target in between.
    #[cfg(unix)]
    fn link_into_prefix(&self, prefix: &Prefix) -> Result<(String, String), String> {
        let base = prefix.drive_c().join(WINE_LINK_DIR);
        std::fs::create_dir_all(&base)
            .map_err(|e| format!("launch: create {}: {e}", base.display()))?;
        let link = |name: &str, target: &Path| -> Result<String, String> {
            let at = base.join(name);
            match std::fs::remove_file(&at) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("launch: replace {}: {e}", at.display())),
            }
            std::os::unix::fs::symlink(target, &at).map_err(|e| {
                format!("launch: link {} -> {}: {e}", at.display(), target.display())
            })?;
            prefix.windows_path(&at).ok_or_else(|| {
                format!(
                    "launch: {} is not under {}, so it has no C: form",
                    at.display(),
                    prefix.drive_c().display()
                )
            })
        };
        Ok((link("overlay", &self.overlay)?, link("state", &self.state_dir)?))
    }

    /// Links every root's host backing directory into `prefix` at the root's
    /// location ([`Prefix::link_location`]), creating the backing directory
    /// first, and records each link for `Drop` to remove.
    ///
    /// Shallowest location first, so a root nested inside another root's
    /// location lands inside the outer root's (already linked) backing
    /// directory rather than creating real directories where the outer link
    /// must go. A location occupied by a real file or directory in the prefix
    /// is refused, never replaced.
    #[cfg(unix)]
    fn link_roots(&self, prefix: &Prefix, roots: &[RootLocation]) -> Result<(), String> {
        let depth = |loc: &str| loc.split(['\\', '/']).filter(|c| !c.is_empty()).count();
        let mut order: Vec<&RootLocation> = roots.iter().collect();
        order.sort_by_key(|r| depth(&r.location));
        for loc in order {
            let backing = self
                .root_backing_dir(loc.id)
                .ok_or_else(|| format!("launch: root {} has no backing directory", loc.id))?;
            std::fs::create_dir_all(&backing)
                .map_err(|e| format!("launch: root {}: create {}: {e}", loc.id, backing.display()))?;
            let link = prefix
                .link_location(&loc.location, &backing)
                .map_err(|e| format!("launch: root {}: {e}", loc.id))?;
            let mut links = self
                .prefix_links
                .lock()
                .map_err(|_| "prefix links lock poisoned".to_string())?;
            links.retain(|(d, l, _)| !(*d == prefix.dir && *l == link));
            links.push((prefix.dir.clone(), link, backing));
        }
        Ok(())
    }

    /// Launch `opts.image` under GE-Proton with the shim injected, served by
    /// this native director over the file-backed ring [`Session::serve`]
    /// started. Requires [`serve`] first, like the Windows body.
    ///
    /// ## How `image` is resolved
    ///
    /// By [`crate::image::classify_image`] against [`Session::root_locations`]
    /// (see [`LaunchOpts::image`]): relative is root 0; a `C:\…` path inside a
    /// root's location is that root's vpath — a real file in the root's
    /// backing directory is launched as is, and a vpath only root 0's graph
    /// serves is **staged** into root 0's backing directory first, exactly as
    /// on Windows (`CreateProcess` inside Wine reads the image, and the loader
    /// resolves its static imports, before any hook of ours exists); a `C:\…`
    /// path outside every root is a prefix program, launched as given. An
    /// absolute host path is accepted only inside `virtual_root`.
    /// `stage_also` / `stage_fallback_dirs` apply to staging as on Windows.
    ///
    /// ## What a Wine launch needs that a Windows one does not
    ///
    /// 1. **A runtime.** The newest verified GE-Proton under this host's
    ///    aether-vfs home ([`Session::set_home`], else `VFS_HOME`, else
    ///    `XDG_DATA_HOME`, else `$HOME` — `vfs_proton::layout::Root::from_env`).
    ///    Never a fallback to stock Proton: `vfs_proton::launch::run`
    ///    re-verifies before it spawns.
    /// 2. **A prefix**: the persistent one [`Session::set_prefix_name`]
    ///    selected, else an anonymous one keyed by `state_dir`
    ///    ([`Session::wine_session_id`]) that this session deletes when it
    ///    drops. Held under an exclusive lock for the launch, so a second live
    ///    session on the same prefix fails fast instead of relinking roots
    ///    under this one.
    /// 3. **`C:\` names for the session's directories**: every root's backing
    ///    directory is symlinked into `drive_c` at the root's location
    ///    ([`Session::link_roots`]; removed on drop), the overlay and state
    ///    directory under `C:\vfs-session` ([`Session::link_into_prefix`]) —
    ///    and `shim.cfg` written *here* rather than in `serve`, since it
    ///    carries root 0's location.
    /// 4. **This ring's real geometry**, taken straight off the live
    ///    [`IpcServe`]: `map_bytes`, `arena_offset`, `arena_len`,
    ///    `payload_cap`. The shim defaults `VFS_RING_BYTES` to 2 MiB, and that
    ///    default over a ~34 MiB ring attaches cleanly, answers a 256 KiB read
    ///    and fails only at 4 MiB — measured, see `vfs_proton::launch`.
    ///    Nothing here may pass a default in their place.
    /// 5. **A running Steam client, for a Steam game.** In a
    ///    [`PrefixInit::Proton`] prefix with an app id, the injector starts
    ///    Proton's Steam helper before the program and the environment
    ///    carries what Steam's own launcher sets
    ///    (`vfs_proton::launch::launch_env`), so the program's Steam API
    ///    finds the client. Without a running client the launch is the same
    ///    as before, plus one line saying so, except that the injector clears
    ///    a stale helper pid — see [`Session::set_steam_helper`].
    ///
    /// `wait: false` returns `Ok(0)` once the program is started, and the
    /// session holds the launch: [`Session::stop_launch`] stops it, and
    /// dropping the session stops it. A launch that is waited on can be
    /// stopped from another thread with [`Session::stop_launch`] too; it then
    /// returns `Ok(`[`STOPPED_EXIT_CODE`]`)`.
    #[cfg(unix)]
    pub fn launch(&self, opts: &LaunchOpts) -> Result<i32, String> {
        let mut handle = self.launch_detached(opts)?;
        if !opts.wait {
            *self
                .detached
                .lock()
                .map_err(|_| "detached launch lock poisoned".to_string())? = Some(handle);
            return Ok(0);
        }
        let stopper = handle.stopper();
        *self
            .waiting
            .lock()
            .map_err(|_| "waiting launch lock poisoned".to_string())? = Some(stopper);
        // Not `handle.wait()`: that consumes `handle`, so its `_prefix_lock`
        // (and the prefix it guards) is free the instant it returns — before
        // this function regains control to clear `waiting` below. A
        // `stop_launch` or a new `launch` landing in that gap would see a
        // stale `waiting` for a launch whose prefix may already be a later
        // one's. Blocking in place keeps `handle`, and so the lock, alive
        // until `waiting` is cleared first.
        let exit = handle.block();
        if let Ok(mut w) = self.waiting.lock() {
            *w = None;
        }
        drop(handle);
        Ok(match exit? {
            LaunchExit::Exited(code) => code,
            LaunchExit::Stopped => STOPPED_EXIT_CODE,
        })
    }

    /// Unix: [`Session::launch`] without waiting, handing the running launch
    /// back. `opts.wait` is ignored. The handle holds the prefix's lock until
    /// the launch ends, and dropping it while the program runs stops it —
    /// keep it (and this session, whose ring the program reads through) for
    /// as long as the program should run.
    #[cfg(unix)]
    pub fn launch_detached(&self, opts: &LaunchOpts) -> Result<LaunchHandle, String> {
        let ipc = self
            .ipc
            .as_ref()
            .ok_or_else(|| "serve() before launch()".to_string())?;
        // `serve` on this target always starts file-backed, so this is really a
        // check that the ring belongs to that `serve` and not to a named
        // section somebody else handed this session.
        let ring = ipc
            .ring_path()
            .ok_or_else(|| {
                "launch: the live ring has no file, so it is not the file-backed ring \
                 serve() starts on this target — a Wine child can only reach a ring by path"
                    .to_string()
            })?
            .to_path_buf();

        if opts.image.trim().is_empty() {
            return Err("LaunchOpts.image is empty — name the image to launch".to_string());
        }
        // A detached launch that has ended still holds the prefix lock.
        self.reap_detached();
        // Refused up front, before `resolve_launch_image` can stage anything:
        // staging replaces `self.staged`, and the old `StagedDir`'s `Drop`
        // deletes the running launch's staged files out from under it —
        // `prefix.lock()` further down would refuse this launch anyway, but
        // only after that damage is done. `starting.swap` both checks and
        // claims "a launch_detached is in flight" in one step, so two calls
        // racing each other (neither yet recorded in `detached`/`waiting`)
        // can't both pass.
        if self
            .detached
            .lock()
            .map_err(|_| "detached launch lock poisoned".to_string())?
            .is_some()
            || self
                .waiting
                .lock()
                .map_err(|_| "waiting launch lock poisoned".to_string())?
                .is_some()
            // A handle `launch_detached` handed back and its caller kept is
            // in neither slot above; `latest` still sees it.
            || self.latest_running().is_some()
            || self.starting.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(
                "launch: a launch is already running in this session — stop_launch() first"
                    .to_string(),
            );
        }
        let _starting = StartingGuard { starting: &self.starting, stop_pending: &self.stop_pending };

        // Before the runtime lookup: a bad image fails fast, and staging
        // behaves as on Windows.
        let resolved = self.resolve_launch_image(opts)?;

        let home = self.proton_home()?;
        // `installed_dirs`, not `installed` + `runtime_dir`: the tag comes from
        // the tree's `version` file and the directory name from the release it
        // was installed from, and re-joining the tag onto `runtimes()` assumes
        // those always agree.
        let runtime = vfs_proton::runtime::installed_dirs(&home)
            .map_err(|e| format!("launch: reading {}: {e}", home.runtimes().display()))?
            .into_iter()
            .next()
            .map(|(_tag, dir)| dir)
            .ok_or_else(|| {
                format!(
                    "launch: no verified GE-Proton runtime under {} — install one with \
                     `vfs-proton install` (VFS_HOME selects where it lands). Launching on \
                     stock Proton instead is the silent downgrade this path refuses.",
                    home.runtimes().display()
                )
            })?;

        let prefix_id = match &self.prefix_name {
            Some(name) => name.clone(),
            None => self.wine_session_id(),
        };
        let prefix_dir = vfs_proton::prefix::prefix_dir(&home, &prefix_id, &self.prefix_init)
            .map_err(|e| format!("launch: wine prefix: {e}"))?;
        if self.prefix_name.is_none() {
            // Recorded before `ensure`, so a boot that fails half-way is
            // still deleted on drop.
            // With the home and runtime used here, so `Drop` deletes this
            // prefix from where it is rather than wherever the environment
            // points by then.
            *self
                .anon
                .lock()
                .map_err(|_| "anon prefix lock poisoned".to_string())? = Some(AnonPrefix {
                id: prefix_id.clone(),
                home: home.clone(),
                runtime: runtime.clone(),
                prefix_dir: prefix_dir.clone(),
            });
        }
        // Locked before `ensure_with`: setting a prefix up — and a Proton
        // prefix recorded by another runtime is set up *again* — must not
        // run under a program another process is running in it.
        let prefix_lock = Prefix { dir: prefix_dir }
            .lock()
            .map_err(|e| format!("launch: {e}"))?;
        let prefix = vfs_proton::prefix::ensure_with(&home, &runtime, &prefix_id, &self.prefix_init)
            .map_err(|e| format!("launch: wine prefix: {e}"))?;

        let (wine_overlay, wine_state) = self.link_into_prefix(&prefix)?;
        let roots = self.root_locations();
        self.link_roots(&prefix, &roots)?;

        // The ring as the shim sees it. Its bytes are the same inode `serve`
        // created; only the name differs.
        let ring_name = ring
            .file_name()
            .ok_or_else(|| format!("launch: ring path {} has no file name", ring.display()))?;
        let wine_ring = join_wine(&wine_state, Path::new(ring_name))?;

        let target = match resolved {
            ResolvedImage::InRoot { root, vpath, .. } => {
                let loc = roots
                    .iter()
                    .find(|r| r.id == root)
                    .ok_or_else(|| format!("launch: root {root} has no location"))?;
                image::join_location(&loc.location, &vpath)
            }
            ResolvedImage::Outside(p) => p,
        };
        let root0 = roots[0].location.clone();
        let cwd = wine_cwd(opts.cwd.as_deref(), &root0, &target)?;
        let extra: Vec<(u32, String)> =
            roots[1..].iter().map(|r| (r.id, r.location.clone())).collect();

        // Written here, not in `serve`: root 0's location and the overlay *as
        // the shim sees them*, and the overlay had no `C:\` name until the
        // prefix above did. The snapshot must still be a valid empty tree —
        // `Engine::build` rejects zero-length snapshot bytes, which would abort
        // dual-layer bootstrap before hooks install.
        let config_path = self.state_dir.join("shim.cfg");
        let snap = empty_tree_snapshot();
        std::fs::write(
            &config_path,
            vfs_protocol::shimcfg::encode_config_with_overlay(&root0, &wine_overlay, &snap),
        )
        .map_err(|e| format!("launch: write {}: {e}", config_path.display()))?;

        let ready_path = self.state_dir.join("ready.flag");
        let _ = std::fs::remove_file(&ready_path);

        let (injector, shim_dll, payload_dll) = locate_wine_artifacts(opts)?;
        let (steam, mut notes) = self.steam_launch(&opts.env);
        // Under the prefix lock taken above, like the rest of the prefix's
        // setup. `PROTON_DISABLE_NVAPI` turns it off as it does for the
        // script: the launch's own value, else this process's. A file that
        // cannot be put in place or taken out is a note, not a failed launch:
        // the program runs without NVAPI, and the script only logs these too.
        let host_disable = std::env::var("PROTON_DISABLE_NVAPI").ok();
        let disable = opts
            .env
            .get("PROTON_DISABLE_NVAPI")
            .map(String::as_str)
            .or(host_disable.as_deref());
        let nvapi = if opts.nvapi && !vfs_proton::nvapi::disabled_by(disable) {
            vfs_proton::nvapi::setup(&vfs_proton::nvapi::Host::real(), &runtime)
        } else {
            if let Err(e) = vfs_proton::nvapi::remove(&prefix.dir) {
                notes.push(format!(
                    "nvapi: could not remove NVAPI from the prefix: {e}"
                ));
            }
            None
        };
        if let Some(nv) = &nvapi {
            for failed in nv.install(&prefix.dir).failed {
                notes.push(format!("nvapi: could not install {failed}"));
            }
        }

        let wine = WineLaunch {
            runtime: runtime.clone(),
            prefix: prefix.dir.clone(),
            injector,
            shim_dll,
            payload_dll,
            target,
            config_file: config_path,
            ready_file: ready_path,
            ring_path: PathBuf::from(wine_ring),
            // The host spelling, for the ring-length check in `spawn`.
            ring_host_path: Some(ring.clone()),
            // The live ring's own numbers. `map_bytes` is the whole mapping
            // (control ring + arena), which is what the shim must map.
            ring_bytes: ipc.map_bytes,
            arena_offset: ipc.arena_offset,
            arena_len: ipc.arena_len,
            payload_cap: ipc.payload_cap,
            virtual_dir: root0,
            virtual_roots: extra,
            args: opts.args.clone(),
            // Child-only: the spawned `wine` gets these in its environment
            // block, and this process's environment is never written.
            extra_env: opts.env.clone(),
            cwd: Some(cwd),
            ready_timeout_secs: opts.ready_timeout.map(|d| d.as_secs().max(1)),
            log_file: opts.log_file.clone(),
            steam,
            notes,
            nvapi,
            registry: self.registry_attached(),
        };

        let child = vfs_proton::launch::spawn(&wine).map_err(|e| format!("launch: {e}"))?;
        let handle = LaunchHandle {
            child,
            stopper: LaunchStopper(Arc::new(StopInner {
                prefix,
                runtime,
                requested: std::sync::atomic::AtomicBool::new(false),
                ended: Mutex::new(false),
            })),
            wine,
            wine_status: None,
            quiet: None,
            outcome: None,
            _prefix_lock: prefix_lock,
        };
        // Before `starting` clears (the guard drops on return), so there is
        // no moment in which a second launch sees neither.
        *self
            .latest
            .lock()
            .map_err(|_| "latest launch lock poisoned".to_string())? =
            Arc::downgrade(&handle.stopper.0);
        if self.stop_pending.swap(false, std::sync::atomic::Ordering::SeqCst) {
            // `stop_launch` ran while this launch was still between spawning
            // and being recorded in `detached`/`waiting`, found nothing to
            // act on, and left this instead of losing the request — honour
            // it now rather than handing back a handle for a program that
            // was supposed to never run.
            let _ = handle.stop();
            return Err(
                "launch: stopped during startup (stop_launch was called before the launch \
                 finished starting)"
                    .to_string(),
            );
        }
        Ok(handle)
    }

    /// Unix: stops the session's running launch — the one a `wait: false`
    /// [`Session::launch`] started, or the one a waiting `launch` on another
    /// thread is blocked on — by stopping its prefix's `wineserver`, which
    /// ends every Wine process in the prefix. `Ok(false)` when nothing is
    /// recorded as running. The waiting `launch` then returns
    /// `Ok(`[`STOPPED_EXIT_CODE`]`)`.
    ///
    /// A launch between spawning and being recorded here (inside
    /// [`Session::launch_detached`], before it returns) has no handle yet for
    /// this to reach: that window returns `Ok(false)` too, but leaves a
    /// pending-cancel flag `launch_detached` checks right after spawning, so
    /// the request still lands rather than being silently lost to the race.
    #[cfg(unix)]
    pub fn stop_launch(&self) -> Result<bool, String> {
        let detached = self
            .detached
            .lock()
            .map_err(|_| "detached launch lock poisoned".to_string())?
            .take();
        if let Some(mut h) = detached {
            if h.is_running() {
                h.stop()?;
                return Ok(true);
            }
        }
        let waiting = self
            .waiting
            .lock()
            .map_err(|_| "waiting launch lock poisoned".to_string())?
            .clone();
        if let Some(stopper) = waiting {
            return stopper.stop().map(|()| true);
        }
        // A handle the caller of `launch_detached` kept.
        if let Some(stopper) = self.latest_running() {
            return stopper.stop().map(|()| true);
        }
        if self.starting.load(std::sync::atomic::Ordering::SeqCst) {
            self.stop_pending.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(false)
    }

    /// The most recent launch, if it has not ended — see [`Session::latest`].
    #[cfg(unix)]
    fn latest_running(&self) -> Option<LaunchStopper> {
        let inner = self.latest.lock().ok()?.upgrade()?;
        let stopper = LaunchStopper(inner);
        (!stopper.has_ended()).then_some(stopper)
    }

    /// Drops a detached launch that has ended, releasing its prefix lock.
    #[cfg(unix)]
    fn reap_detached(&self) {
        if let Ok(mut d) = self.detached.lock() {
            if d.as_mut().is_some_and(|h| !h.is_running()) {
                *d = None;
            }
        }
    }

    /// The unix half of `Drop`: stops a still-running launch, then `stop_serve`,
    /// removes the root links `launch` placed and deletes the anonymous prefix.
    pub(super) fn drop_proton(&mut self) {
        // Before the ring goes: a detached program still reads through it.
        let detached = match self.detached.get_mut() {
            Ok(d) => d.take(),
            Err(p) => p.into_inner().take(),
        };
        if let Some(h) = detached {
            let _ = h.stop();
        }
        // A launch whose handle the caller kept reads through this ring
        // too.
        if let Some(stopper) = self.latest_running() {
            let _ = stopper.stop();
        }
        self.stop_serve();
        let links = match self.prefix_links.get_mut() {
            Ok(l) => std::mem::take(l),
            Err(p) => std::mem::take(p.into_inner()),
        };
        // Reverse of link order (shallowest first), so a nested root's
        // link — recorded through the outer root's link — is removed while
        // that path still resolves. `unlink_location` removes only a
        // link the prefix's manifest lists that still points where we
        // pointed it, and delists it.
        for (dir, link, target) in links.into_iter().rev() {
            let _ = Prefix { dir }.unlink_location(&link, &target);
        }
        let anon = match self.anon.get_mut() {
            Ok(a) => a.take(),
            Err(p) => p.into_inner().take(),
        };
        if let Some(AnonPrefix { id, home, runtime, prefix_dir }) = anon {
            // `wineserver` lingers after the child and rewrites the
            // registry into the prefix as it exits; stop it first (bounded)
            // or the deleted prefix comes back.
            let _ = Prefix { dir: prefix_dir }.stop_wineserver(&runtime);
            let _ = vfs_proton::prefix::remove_session(&home, &id);
        }
    }
}

/// The working directory a Proton launch starts `target` in: `requested` as
/// given when it is a Windows absolute path, joined onto root 0's location
/// when it is relative, and `target`'s own directory when it is `None`.
#[cfg(unix)]
fn wine_cwd(requested: Option<&str>, root0: &str, target: &str) -> Result<String, String> {
    match requested {
        Some(c) if c.trim().is_empty() => Err(
            "launch: LaunchOpts.cwd is empty — leave it None for the image's own directory"
                .to_string(),
        ),
        Some(c) if c.split(['\\', '/']).any(|part| part == "..") => Err(format!(
            "launch: LaunchOpts.cwd {c:?} contains '..'; name the directory directly"
        )),
        Some(c) if image::is_windows_absolute(c) => Ok(c.to_string()),
        Some(c) => Ok(image::join_location(root0, c.trim_matches(['\\', '/']))),
        None => Ok(match target.rfind(['\\', '/']) {
            Some(i) if target[..i].ends_with(':') => format!("{}\\", &target[..i]),
            Some(i) => target[..i].to_string(),
            None => root0.to_string(),
        }),
    }
}

/// The ring file [`Session::serve`] creates inside `state_dir` on unix. Named
/// as a constant because [`Session::launch`] has to render the same file as a
/// `C:\` path for the shim, and the two must not drift apart.
#[cfg(unix)]
pub(in crate::session) const RING_FILE: &str = "ring.bin";

/// Where [`Session::launch`] links the session's directories inside the
/// prefix's `drive_c` — see [`Session::link_into_prefix`].
#[cfg(unix)]
const WINE_LINK_DIR: &str = "vfs-session";

/// Root 0's location inside the prefix when none is declared — the path it has
/// always had, `C:\` + [`WINE_LINK_DIR`] + `\root`, so existing hosts and tests
/// see no change.
#[cfg(unix)]
pub(in crate::session) const DEFAULT_ROOT0_LOCATION: &str = r"C:\vfs-session\root";

/// Inline ring payload capacity for the file-backed ring.
///
/// The value the named-section path uses (`vfs_ipc::DEFAULT_PAYLOAD_CAP`),
/// restated because `vfs-embed` does not depend on `vfs-ipc`. Restating it
/// cannot desynchronize the two ends of *this* ring: the child is told this
/// server's capacity from `IpcServe::payload_cap`, never a default at either
/// end. It would only mean a Wine session pipelines differently from a Windows
/// one if the constant there changed.
#[cfg(unix)]
const PROTON_PAYLOAD_CAP: u32 = 1_048_576;

/// Appends `rel`'s components to a `C:\…` prefix with Wine's separator.
///
/// Anything that is not a plain component is **refused, not normalized**: this
/// builds the name the child will open, and `..` would leave the managed root
/// while a root or drive component would produce a path with two prefixes.
/// Quietly normalizing either one points a launch somewhere nobody asked for.
#[cfg(unix)]
fn join_wine(base: &str, rel: &Path) -> Result<String, String> {
    let mut out = base.to_string();
    for c in rel.components() {
        match c {
            std::path::Component::Normal(part) => {
                out.push('\\');
                out.push_str(&part.to_string_lossy());
            }
            other => {
                return Err(format!(
                    "launch: {} cannot be named under {base}: {other:?} is not a plain path \
                     component",
                    rel.display()
                ))
            }
        }
    }
    Ok(out)
}

/// The three Windows binaries a Proton launch needs, resolved and checked, or
/// a message naming exactly which are missing.
///
/// `vfs-injector.exe`, `vfs_shim_dll.dll` and `vfs_payload.dll` are Windows
/// targets, cross-built separately from the Linux host (`bin/build-windows`,
/// which copies them beside the Linux binaries) — so this resolves what is
/// already there rather than producing anything, and says so in the failure. [`LaunchOpts::shim_dll`] / [`LaunchOpts::payload_dll`]
/// win when set; the documented default location is the directory holding
/// `shim_dll` if only that is set, else `VFS_WINDOWS_ARTIFACTS`, else the
/// directory holding `current_exe()`. The injector has no `LaunchOpts` field of its own (adding
/// one is a change to a public struct, which this increment does not make), so
/// that same directory is where it is looked for — the one `cargo build` puts
/// all three in.
///
/// All three are checked before any of them is used, and every missing one is
/// listed: a launch that reported them one at a time would cost a Wine
/// round-trip per file.
#[cfg(unix)]
fn locate_wine_artifacts(opts: &LaunchOpts) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let dir = vfs_env::path(vfs_env::WINDOWS_ARTIFACTS).filter(|p| !p.as_os_str().is_empty());
    locate_wine_artifacts_in(opts, dir.as_deref())
}

/// [`locate_wine_artifacts`] with `VFS_WINDOWS_ARTIFACTS`'s value passed in, so
/// a test can name it without writing the process environment.
#[cfg(unix)]
fn locate_wine_artifacts_in(
    opts: &LaunchOpts,
    env_dir: Option<&Path>,
) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let base = match (&opts.shim_dll, env_dir) {
        (Some(s), _) => Path::new(s)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
        (None, Some(dir)) => dir.to_path_buf(),
        (None, None) => std::env::current_exe()
            .map_err(|e| format!("launch: current_exe: {e}"))?
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "launch: current_exe() has no parent directory".to_string())?,
    };
    let shim = opts
        .shim_dll
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| base.join("vfs_shim_dll.dll"));
    let payload = opts
        .payload_dll
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| base.join("vfs_payload.dll"));
    let injector = base.join("vfs-injector.exe");

    let missing: Vec<String> = [&injector, &shim, &payload]
        .iter()
        .filter(|p| !p.is_file())
        .map(|p| p.display().to_string())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "launch: these Windows artifacts are missing: {}. Cross-build them with \
             `bin/build-windows` (which copies them beside the Linux binaries), put all three \
             in {}, set VFS_WINDOWS_ARTIFACTS to the directory holding them, or set \
             LaunchOpts.shim_dll and LaunchOpts.payload_dll to where they are \
             (vfs-injector.exe is then looked for beside shim_dll).",
            missing.join(", "),
            base.display()
        ));
    }
    Ok((injector, shim, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unix_default_root_zero_location_is_where_the_session_dir_is_linked() {
        assert_eq!(DEFAULT_ROOT0_LOCATION, format!(r"C:\{WINE_LINK_DIR}\root"));
    }

    #[cfg(unix)]
    #[test]
    fn check_root_location_applies_the_link_rule() {
        Session::check_root_location(r"C:\Games\Fixture").unwrap();
        Session::check_root_location("c:/users/steamuser/Saves").unwrap();
        for bad in [r"D:\Games", "/tmp/host-dir", r"C:\", r"C:\a\..\b", "Games"] {
            let e = Session::check_root_location(bad).unwrap_err();
            assert!(e.contains("bad root location") && e.contains(bad), "{bad}: {e}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn set_prefix_name_takes_one_plain_component() {
        let mut s = Session::new();
        for bad in ["", "a/b", "..", "/abs", r"a\b"] {
            assert!(s.set_prefix_name(bad).is_err(), "{bad:?} must be refused");
        }
        s.set_prefix_name("skyrim").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn set_home_replaces_the_environments_home() {
        let mut s = Session::new();
        let home = scratch("home");
        s.set_home(&home);
        assert_eq!(s.proton_home().unwrap(), ProtonRoot::at(home));
    }

    #[cfg(unix)]
    fn scratch(tag: &str) -> PathBuf {
        let p = crate::test_scratch::scratch_created(&format!("lr-{tag}"));
        p
    }

    /// A session with its own host dirs and a scratch (never booted) prefix.
    #[cfg(unix)]
    fn linked_session(tag: &str) -> (Session, Prefix) {
        let mut s = Session::new();
        s.set_root(scratch(&format!("{tag}-root")));
        s.set_state_dir(scratch(&format!("{tag}-state")));
        (s, Prefix { dir: scratch(&format!("{tag}-prefix")) })
    }

    /// Ruling 3: a root nested in another root's location links *inside* the
    /// outer root's backing dir, whichever order the roots arrive in.
    #[cfg(unix)]
    #[test]
    fn nested_roots_link_inside_the_outer_backing_dir_in_either_order() {
        // Root 0 outer, root 1 inner; the slice handed over both ways round.
        for (tag, reverse) in [("n-fwd", false), ("n-rev", true)] {
            let (mut s, prefix) = linked_session(tag);
            s.declare_root(0, r"C:\G");
            s.declare_root(1, r"C:\G\Saves");
            let mut roots = s.root_locations();
            if reverse {
                roots.reverse();
            }
            s.link_roots(&prefix, &roots).unwrap();
            let outer = prefix.drive_c().join("G");
            assert_eq!(std::fs::read_link(&outer).unwrap(), s.virtual_root(), "{tag}");
            let inner = s.virtual_root().join("Saves");
            assert_eq!(
                std::fs::read_link(&inner).unwrap(),
                s.state_dir().join("roots").join("1"),
                "{tag}: the inner link must land inside root 0's backing dir"
            );
            assert!(outer.join("Saves").is_dir(), "{tag}: the inner root resolves through the outer");
        }
        // Two extra roots, inner declared first.
        let (mut s, prefix) = linked_session("n-extra");
        s.declare_root(1, r"C:\G\Saves");
        s.declare_root(2, r"C:\G");
        s.link_roots(&prefix, &s.root_locations()).unwrap();
        let outer_backing = s.state_dir().join("roots").join("2");
        assert_eq!(std::fs::read_link(prefix.drive_c().join("G")).unwrap(), outer_backing);
        assert_eq!(
            std::fs::read_link(outer_backing.join("Saves")).unwrap(),
            s.state_dir().join("roots").join("1")
        );
        // And drop removes both of this session's own links.
        let links = [prefix.drive_c().join("G"), outer_backing.join("Saves")];
        drop(s);
        for l in links {
            assert!(std::fs::symlink_metadata(&l).is_err(), "{} must be removed on drop", l.display());
        }
        assert!(
            prefix.read_manifest().unwrap().is_empty(),
            "drop must delist the links it removed from the prefix's manifest"
        );
    }

    /// A user's own symlink at a root location in a persistent prefix: the
    /// launch refuses it (by name), and it survives the session.
    #[cfg(unix)]
    #[test]
    fn a_foreign_symlink_at_a_root_location_is_refused_and_survives() {
        let (mut s, prefix) = linked_session("foreign");
        s.declare_root(1, r"C:\Games\Skyrim");
        let theirs = scratch("foreign-theirs");
        let at = prefix.drive_c().join("Games").join("Skyrim");
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&theirs, &at).unwrap();
        let e = s.link_roots(&prefix, &s.root_locations()).unwrap_err();
        assert!(e.contains("root 1") && e.contains("did not create"), "{e}");
        drop(s);
        assert_eq!(std::fs::read_link(&at).unwrap(), theirs, "their link must survive");
    }

    /// Another session relinked the same location after this one launched:
    /// this session's drop must leave that link alone.
    #[cfg(unix)]
    #[test]
    fn drop_leaves_a_link_another_session_repointed() {
        let (mut s, prefix) = linked_session("d-repoint");
        s.declare_root(1, r"C:\users\steamuser\Saves");
        s.link_roots(&prefix, &s.root_locations()).unwrap();
        let link = prefix.drive_c().join("users").join("steamuser").join("Saves");
        let theirs = scratch("d-repoint-theirs");
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&theirs, &link).unwrap();
        drop(s);
        assert_eq!(std::fs::read_link(&link).unwrap(), theirs, "another session's link must survive");
    }

    /// Something replaced the recorded link with a real directory: drop must
    /// never remove it or its contents.
    #[cfg(unix)]
    #[test]
    fn drop_leaves_a_real_directory_at_a_recorded_link_path() {
        let (mut s, prefix) = linked_session("d-real");
        s.declare_root(1, r"C:\Games\Mine");
        s.link_roots(&prefix, &s.root_locations()).unwrap();
        let link = prefix.drive_c().join("Games").join("Mine");
        std::fs::remove_file(&link).unwrap();
        std::fs::create_dir(&link).unwrap();
        std::fs::write(link.join("keep.txt"), b"keep").unwrap();
        drop(s);
        assert_eq!(std::fs::read(link.join("keep.txt")).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn an_anonymous_prefix_is_removed_on_drop_and_a_named_one_is_not() {
        let home = crate::test_scratch::scratch_dir("drop-home");
        let root = ProtonRoot::at(home.clone());
        let anon_dir = root.try_session_dir("anon-x").unwrap().join("prefix");
        let named_dir = root.try_session_dir("named-x").unwrap().join("prefix");
        std::fs::create_dir_all(&anon_dir).unwrap();
        std::fs::create_dir_all(&named_dir).unwrap();
        // The environment's home is somewhere else entirely: `Drop` must use
        // the home recorded at launch, never re-read `VFS_HOME`.
        {
            let a = Session::new();
            *a.anon.lock().unwrap() = Some(AnonPrefix {
                id: "anon-x".into(),
                home: root.clone(),
                // No `wineserver` here: stopping it fails fast and is ignored.
                runtime: home.join("no-runtime"),
                prefix_dir: anon_dir.clone(),
            });
            let mut n = Session::new();
            n.set_prefix_name("named-x").unwrap();
        }
        assert!(!anon_dir.exists(), "anonymous prefix must be deleted on drop");
        assert!(named_dir.exists(), "a named prefix is persistent");
    }

    #[cfg(unix)]
    #[test]
    fn windows_artifacts_come_from_the_named_directory_unless_shim_dll_is_set() {
        let dir = scratch("artifacts");
        for n in ["vfs-injector.exe", "vfs_shim_dll.dll", "vfs_payload.dll"] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        let (inj, shim, payload) =
            locate_wine_artifacts_in(&LaunchOpts::default(), Some(&dir)).unwrap();
        assert_eq!(inj, dir.join("vfs-injector.exe"));
        assert_eq!(shim, dir.join("vfs_shim_dll.dll"));
        assert_eq!(payload, dir.join("vfs_payload.dll"));

        // An explicit shim_dll wins over the directory.
        let other = scratch("artifacts-other");
        let opts = LaunchOpts {
            shim_dll: Some(other.join("vfs_shim_dll.dll").to_string_lossy().into_owned()),
            ..Default::default()
        };
        let e = locate_wine_artifacts_in(&opts, Some(&dir)).unwrap_err();
        assert!(e.contains(&other.display().to_string()) && e.contains("VFS_WINDOWS_ARTIFACTS"), "{e}");

        // A directory missing some of them names each one.
        std::fs::remove_file(dir.join("vfs_payload.dll")).unwrap();
        let e = locate_wine_artifacts_in(&LaunchOpts::default(), Some(&dir)).unwrap_err();
        assert!(e.contains("vfs_payload.dll") && !e.contains("vfs-injector.exe,"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn wine_cwd_defaults_to_the_image_directory() {
        assert_eq!(wine_cwd(None, r"C:\G", r"C:\G\bin\x.exe").unwrap(), r"C:\G\bin");
        assert_eq!(wine_cwd(None, r"C:\G", r"C:\x.exe").unwrap(), r"C:\");
        assert_eq!(wine_cwd(None, r"C:\G", "C:/tools/probe.exe").unwrap(), "C:/tools");
    }

    #[cfg(unix)]
    #[test]
    fn wine_cwd_takes_absolute_paths_as_given_and_relative_ones_under_root_zero() {
        assert_eq!(wine_cwd(Some(r"D:\x"), r"C:\G", r"C:\G\a.exe").unwrap(), r"D:\x");
        assert_eq!(wine_cwd(Some("Data/SKSE/"), r"C:\G", r"C:\G\a.exe").unwrap(), r"C:\G\Data\SKSE");
        assert_eq!(wine_cwd(Some(r"\Data"), r"C:\G\", r"C:\G\a.exe").unwrap(), r"C:\G\Data");
    }

    #[cfg(unix)]
    #[test]
    fn wine_cwd_refuses_empty_and_dot_dot() {
        for bad in ["", "  ", r"..\x", r"C:\G\..\Windows"] {
            let e = wine_cwd(Some(bad), r"C:\G", r"C:\G\a.exe").unwrap_err();
            assert!(e.contains("cwd"), "{bad:?}: {e}");
        }
    }
}
