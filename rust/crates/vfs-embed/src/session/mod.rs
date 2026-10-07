//! Host session: configure mounts + paths, serve IPC, **launch a process** with
//! all NT I/O under the virtual root remapped through this director.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// `vfs_director::ipc` is portable, and `Session` now uses **both** of its
// halves: the named-section handshake on Windows (`IpcServe::start`, the event
// pair, `write_thin_config`, `apply_env_roots`) and the file-backed ring on
// unix (`IpcServe::start_file_backed`), which is how a shim inside Wine reaches
// a native Linux director. So neither this import nor the `ipc` field below is
// gated; only the two bodies that pick a transport are.
use vfs_director::ipc::IpcServe;
#[cfg(unix)]
use crate::image;
use crate::image::RootLocation;
use vfs_director::stage::StagedDir;
use vfs_director::Director;
// The Proton delivery mechanism: the unix counterpart of the `vfs-inject` +
// `vfs-shim` pair, carrying GE-Proton discovery, the per-session Wine prefix,
// and the injector's positional argv plus the shim's env handshake. Gated in
// the manifest too (`[target.'cfg(unix)'.dependencies]`).
#[cfg(unix)]
use vfs_proton::{
    launch::WineLaunch,
    layout::Root as ProtonRoot,
    prefix::{Prefix, PrefixInit, PrefixLock},
    steam::SteamSide,
};
use vfs_provider::{
    overlay_layer_dir, RootId,
};

mod compose;
pub use compose::compose_root;
use compose::RootComposition;
mod registry;
pub use registry::{registry_sync_for, RegistrySync};
mod read;
mod stage;
#[cfg(unix)]
use stage::{empty_tree_snapshot, ResolvedImage};
#[cfg(windows)]
mod windows;
mod opts;
pub use opts::{LaunchOpts, StageOpts};

/// Host entrypoint: one configured director + optional IPC + launch.
///
/// Typical use:
/// 1. `Session::new` + `set_root` / `set_overlay` / `set_state_dir`
/// 2. `mount` backends (zip/disk/C)
/// 3. `serve` — start ring so the child shim can talk to us
/// 4. `launch` — CreateProcess + inject; child I/O under root is remapped
pub struct Session {
    kernel: Arc<Director>,
    virtual_root: PathBuf,
    overlay: PathBuf,
    state_dir: PathBuf,
    /// The live ring, on both targets: a named section on Windows, a real file
    /// under `state_dir` on unix. One field rather than a `bool` beside it,
    /// because [`Session::is_serving`] and [`Session::stop_serve`] then have
    /// one thing to consult and cannot disagree with each other by target.
    ipc: Option<IpcServe>,
    /// The ring's file when it lives in memory rather than in `state_dir`
    /// (see [`Session::serve`]), so that [`Session::stop_serve`] can delete
    /// it: left behind it would hold its pages until the user logs out.
    #[cfg(unix)]
    ring_backing: Option<PathBuf>,
    /// Per-root composition inputs, keyed by the raw `u32` a `RootId` wraps.
    /// `Director` holds exactly one provider per root rather than a mergeable
    /// list, so every change to a root's inputs recomposes that root whole
    /// (see [`Session::recompose`]).
    ///
    /// **This is the one place a session's provider graph is composed**, for
    /// every root and for every host: `Session::mount`'s single-root
    /// convenience, and `vfs-directord`'s `SessionRegistry` (which drives the
    /// multi-root gRPC/TOML surface) both land here. Composing anywhere else
    /// — calling `kernel().mount` with a hand-built graph — silently drops
    /// whatever the *other* half of the composition contributed, which is
    /// exactly how the daemon surface lost copy-on-write while the harness
    /// kept it (gate 4, Task 6b).
    roots: Mutex<BTreeMap<u32, RootComposition>>,
    /// The session's roots **beyond root 0**, as declared — see
    /// [`Session::declare_root`]. On Windows each is the host directory the
    /// root virtualizes; on unix it is the root's **location** inside the Wine
    /// prefix (`C:\…`), backed by `state_dir/roots/<id>`.
    extra_roots: Vec<(u32, PathBuf)>,
    /// Root 0's location as the Wine child sees it, when declared; `None` is
    /// [`DEFAULT_ROOT0_LOCATION`]. `virtual_root` stays its **host** backing
    /// directory.
    #[cfg(unix)]
    root0_location: Option<String>,
    /// A persistent prefix name ([`Session::set_prefix_name`]); `None` boots
    /// an anonymous prefix keyed by `state_dir` and deleted on drop.
    #[cfg(unix)]
    prefix_name: Option<String>,
    /// The anonymous prefix this session booted, if any — what `Drop`
    /// deletes, with the home and runtime `launch` used for it. A named
    /// prefix is never recorded here.
    #[cfg(unix)]
    anon: Mutex<Option<AnonPrefix>>,
    /// `(prefix dir, link, target)` for every root link `launch` placed in a
    /// prefix, so `Drop` can remove them through
    /// [`Prefix::unlink_location`] — only while each is still a symlink to
    /// what we linked, so a later session's relink of the same location
    /// survives.
    #[cfg(unix)]
    prefix_links: Mutex<Vec<(PathBuf, PathBuf, PathBuf)>>,
    /// Ring serve threads [`Session::serve`] starts; `None` is
    /// `vfs_director::ipc::DEFAULT_IO_WORKERS`.
    io_workers: Option<usize>,
    /// The aether-vfs home [`Session::set_home`] chose; `None` resolves it
    /// from the environment at launch.
    #[cfg(unix)]
    home: Option<PathBuf>,
    /// How `launch` sets up the prefix — see [`Session::set_prefix_init`].
    #[cfg(unix)]
    prefix_init: PrefixInit,
    /// Whether `launch` starts Proton's Steam helper — see
    /// [`Session::set_steam_helper`].
    #[cfg(unix)]
    steam_helper: bool,
    /// Where the Steam client keeps `steam.pid`; `None` is `$HOME/.steam` —
    /// see [`Session::set_steam_state_dir`].
    #[cfg(unix)]
    steam_state_dir: Option<PathBuf>,
    /// The launch `launch` is waiting on, for [`Session::stop_launch`].
    #[cfg(unix)]
    waiting: Mutex<Option<LaunchStopper>>,
    /// The launch a `wait: false` `launch` started, held until it is
    /// stopped, replaced by a later launch after it ended, or the session
    /// drops (which stops it).
    #[cfg(unix)]
    detached: Mutex<Option<LaunchHandle>>,
    /// Set for the span of a [`Session::launch_detached`] call, from just
    /// after it refuses a second launch to just before it returns — the
    /// window in which neither `detached` nor `waiting` yet holds the
    /// handle, so [`Session::stop_launch`] has nothing to act on directly.
    /// Doubles as the second half of that refusal: a `launch_detached` that
    /// finds it already set (another one is mid-flight) refuses too.
    #[cfg(unix)]
    starting: std::sync::atomic::AtomicBool,
    /// Set by [`Session::stop_launch`] when it finds `starting` set but
    /// nothing in `detached`/`waiting` yet — a stop requested while a launch
    /// is between spawning and being recorded. `launch_detached` checks this
    /// the moment it has a handle, right after spawning, so the request is
    /// honoured instead of silently lost to that race.
    #[cfg(unix)]
    stop_pending: std::sync::atomic::AtomicBool,
    /// The most recent Proton launch's stop state, weakly: whoever holds the
    /// launch — `detached`, a waiting `launch`, or a caller of
    /// [`Session::launch_detached`] that kept its [`LaunchHandle`] — this
    /// sees it while it runs. A new launch is refused while it has not
    /// ended, [`Session::stop_launch`] stops it, and dropping the session
    /// stops it before the ring goes.
    #[cfg(unix)]
    latest: Mutex<std::sync::Weak<StopInner>>,
    /// The most recent staged launch directory, held here because
    /// [`StagedDir`]'s `Drop` removes the staged files — not the virtual root
    /// they now live in — and Windows keeps the image file mapped for as long
    /// as the child runs. Dropped with the session, or replaced by the next
    /// staged launch.
    staged: Mutex<Option<StagedDir>>,
}

impl Session {
    /// A session with default `root`/`overlay`/`state` directories under
    /// `%TEMP%`, which a host normally replaces via
    /// [`Session::set_root`] / [`Session::set_overlay`] /
    /// [`Session::set_state_dir`].
    ///
    /// The defaults are unique per session **and cleared on the way in**, for
    /// the reason spelled out on [`Session::set_overlay`]: every component of
    /// a temp name repeats across runs (the OS recycles pids; the counter
    /// below restarts at zero in each process) and nothing deletes a session's
    /// directory when its owner dies, so an inherited `overlay/root-0` breaks
    /// the "the overlay is empty afterwards" check this project uses to detect
    /// a write that bypassed the director. Warning hosts about that while
    /// shipping a default that has it would be advice this crate does not take
    /// itself. The path is this session's alone, so clearing it cannot destroy
    /// anything a caller put there — it has had no chance to.
    pub fn new() -> Self {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir()
            .join(format!("vfs-session-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        Session {
            kernel: Arc::new(Director::new()),
            virtual_root: tmp.join("root"),
            overlay: tmp.join("overlay"),
            state_dir: tmp.join("state"),
            ipc: None,
            roots: Mutex::new(BTreeMap::new()),
            extra_roots: Vec::new(),
            #[cfg(unix)]
            root0_location: None,
            #[cfg(unix)]
            prefix_name: None,
            #[cfg(unix)]
            anon: Mutex::new(None),
            #[cfg(unix)]
            prefix_links: Mutex::new(Vec::new()),
            io_workers: None,
            #[cfg(unix)]
            home: None,
            #[cfg(unix)]
            prefix_init: PrefixInit::default(),
            #[cfg(unix)]
            steam_helper: true,
            #[cfg(unix)]
            steam_state_dir: None,
            #[cfg(unix)]
            waiting: Mutex::new(None),
            #[cfg(unix)]
            detached: Mutex::new(None),
            #[cfg(unix)]
            latest: Mutex::new(std::sync::Weak::new()),
            #[cfg(unix)]
            starting: std::sync::atomic::AtomicBool::new(false),
            #[cfg(unix)]
            stop_pending: std::sync::atomic::AtomicBool::new(false),
            staged: Mutex::new(None),
            #[cfg(unix)]
            ring_backing: None,
        }
    }

    /// The number of ring serve threads [`Session::serve`] starts, from the
    /// next `serve` on. The default is `vfs_director::ipc::DEFAULT_IO_WORKERS`
    /// (4); the count is clamped to 1..=32 (the ring's slot count).
    ///
    /// A serve thread answers one request at a time, so a request whose
    /// provider blocks — a cold miss behind a network-backed cache — holds a
    /// thread until it returns, and once every thread is held the program's
    /// I/O stops, cache hits included. Raise this above the number of slow
    /// misses a host expects at once. Each idle thread costs a little CPU:
    /// it spins briefly after activity before it sleeps.
    pub fn set_io_workers(&mut self, n: usize) {
        self.io_workers = Some(n);
    }

    /// The serve thread count the next [`Session::serve`] uses, clamped.
    pub fn io_workers(&self) -> usize {
        vfs_director::ipc::clamp_workers(
            self.io_workers.unwrap_or(vfs_director::ipc::DEFAULT_IO_WORKERS),
        )
    }

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

    pub fn kernel(&self) -> &Arc<Director> {
        &self.kernel
    }

    pub fn set_root(&mut self, path: impl Into<PathBuf>) {
        self.virtual_root = path.into();
    }

    /// Where the injected shim's local write overlay lands on disk.
    ///
    /// [`Session::serve`] creates this directory; it does **not** empty it,
    /// and nothing removes it when the process that owned it dies. A host that
    /// derives the path from anything repeatable — a pid, a counter that
    /// restarts at zero, a fixed name — will eventually hand a new session a
    /// previous run's overlay.
    ///
    /// That is not housekeeping. "The overlay is empty afterwards" is how this
    /// project detects a write that bypassed the director, so inherited
    /// content fails that check with nothing having actually fallen through —
    /// and, worse in the other direction, a real bypass gets dismissed as
    /// leftovers. `vfs-directord`'s `SessionRegistry::create` clears its base
    /// directory before every session for exactly this reason. A host picking
    /// its own directories inherits the hazard along with the choice.
    pub fn set_overlay(&mut self, path: impl Into<PathBuf>) {
        self.overlay = path.into();
    }

    pub fn set_state_dir(&mut self, path: impl Into<PathBuf>) {
        self.state_dir = path.into();
    }

    pub fn virtual_root(&self) -> &Path {
        &self.virtual_root
    }

    /// The physical subdirectory of this session's overlay that `root`'s
    /// writes actually land in — see `vfs_shim::overlay_layer_dir`, which
    /// this delegates to, and `Overlay::root_dir` on the shim side (the
    /// same directory `Engine`'s local write overlay resolves against).
    ///
    /// A host mounting its *own* read layer over the overlay directory
    /// (e.g. a `DiskProvider`, so the director sees content the shim's
    /// overlay has written — see `vfs-directord/src/bin/skyrim-live.rs`)
    /// must mount exactly this path, not [`Session::set_overlay`]'s bare
    /// path: the shim's overlay is root-scoped on disk (gate 4, Task 2), so
    /// mounting the bare overlay directory would show nothing the overlay
    /// has actually written, and any writer/reader pair that disagrees on
    /// this path silently desyncs.
    pub fn overlay_layer_dir(&self, root: RootId) -> PathBuf {
        overlay_layer_dir(&self.overlay, root)
    }

    /// Declare a managed root: the host directory that `RootId(id)`
    /// virtualizes.
    ///
    /// This is the *shim-facing* half of a multi-root session and it is
    /// separate from mounting a provider on that root
    /// (`kernel().mount(RootId(id), …)`) on purpose, because they answer
    /// different questions: the mount says what root `n` serves, this says
    /// which real filesystem location the injected process should recognise
    /// *as* root `n`. Declare without mounting and the root serves nothing;
    /// mount without declaring and the shim never classifies any path into
    /// that root at all, so every path under it falls through to real disk —
    /// silently, which is why this is not optional plumbing.
    ///
    /// **Root 0 is [`Session::set_root`]**, and declaring it here does exactly
    /// that rather than being recorded separately. Root 0's host directory is
    /// `virtual_root` — there is no second place to keep it — so a host that
    /// walks its roots and declares all of them, id 0 included, gets the
    /// meaning it asked for. It previously did not: the call was accepted,
    /// stored in [`Session::declared_roots`], and then dropped on the way to
    /// the environment the child inherits, so root 0 silently stayed wherever
    /// `set_root` had left it. Accepting-then-discarding is the one behaviour
    /// that cannot be right, and a fallible signature would force every host
    /// to special-case the id that needs it least.
    ///
    /// A daemon session declares root 0 the same way when its config names
    /// one (`SessionRegistry::declare_root`), and reports the declared
    /// location as the session's root.
    ///
    /// Re-declaring an id replaces its path. Takes effect at the next
    /// [`Session::serve`] or [`Session::launch`], which is what publishes it
    /// into the environment the child inherits.
    ///
    /// **On unix `path` is a location, not a host directory**: the `C:\…`
    /// path the Wine child sees the root at. `launch` backs it with a host
    /// directory (root 0: `virtual_root`, still set by [`Session::set_root`];
    /// root N: `state_dir/roots/N`) symlinked into the prefix at that
    /// location. So on unix `declare_root(0, …)` sets root 0's **location**
    /// and leaves `virtual_root` alone. See [`Session::root_locations`].
    pub fn declare_root(&mut self, id: u32, path: impl Into<PathBuf>) {
        let path = path.into();
        if id == 0 {
            #[cfg(unix)]
            {
                self.root0_location = Some(path.to_string_lossy().into_owned());
            }
            #[cfg(not(unix))]
            {
                self.virtual_root = path;
            }
            return;
        }
        match self.extra_roots.iter_mut().find(|(r, _)| *r == id) {
            Some(slot) => slot.1 = path,
            None => self.extra_roots.push((id, path)),
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

    /// The roots declared beyond root 0, in declaration order. For
    /// diagnostics and for tests that need to prove a config's `[[root]]`
    /// table actually reached the session rather than being parsed and
    /// dropped.
    pub fn declared_roots(&self) -> &[(u32, PathBuf)] {
        &self.extra_roots
    }

    /// Every root's **location** — the path the launched program sees it at —
    /// root 0 first, then the extra roots in declaration order.
    ///
    /// On Windows root 0's location is `virtual_root` and each extra root's is
    /// its declared host directory. On unix root 0's is its declared location,
    /// else [`DEFAULT_ROOT0_LOCATION`], and each extra root's is its declared
    /// `C:\…` location. [`crate::image::classify_image`] resolves launch
    /// images against exactly this list.
    pub fn root_locations(&self) -> Vec<RootLocation> {
        #[cfg(unix)]
        let root0 = self
            .root0_location
            .clone()
            .unwrap_or_else(|| DEFAULT_ROOT0_LOCATION.to_string());
        #[cfg(not(unix))]
        let root0 = self.virtual_root.to_string_lossy().into_owned();
        std::iter::once(RootLocation { id: 0, location: root0 })
            .chain(self.extra_roots.iter().map(|(id, p)| RootLocation {
                id: *id,
                location: p.to_string_lossy().into_owned(),
            }))
            .collect()
    }

    /// The host directory that backs `root`: root 0's is `virtual_root`; a
    /// declared extra root's is its declared path on Windows and
    /// `state_dir/roots/<id>` on unix. `None` for an undeclared root.
    fn root_backing_dir(&self, root: u32) -> Option<PathBuf> {
        if root == 0 {
            return Some(self.virtual_root.clone());
        }
        let (_, declared) = self.extra_roots.iter().find(|(id, _)| *id == root)?;
        #[cfg(unix)]
        {
            let _ = declared;
            Some(self.state_dir.join("roots").join(root.to_string()))
        }
        #[cfg(not(unix))]
        {
            Some(declared.clone())
        }
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

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Whether IPC workers are running (required before [`launch`]).
    pub fn is_serving(&self) -> bool {
        self.ipc.is_some()
    }

    /// Access the live IPC server (after [`serve`]) for probes / diagnostics.
    ///
    /// Portable, now that both transports are wired: `IpcServe` exists on unix
    /// too, holding a file-backed ring. This is also the honest way to read a
    /// session's ring **geometry** — `map_bytes`, `arena_offset`, `arena_len`,
    /// `payload_cap` are exactly what a child must be told, and a default in
    /// their place is silent at attach and fatal under load (see
    /// `vfs_proton::launch`). A host that needs only *whether* a session is
    /// serving should use [`Session::is_serving`].
    pub fn ipc(&self) -> Option<&IpcServe> {
        self.ipc.as_ref()
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

    pub fn stop_serve(&mut self) {
        if let Some(ipc) = self.ipc.take() {
            ipc.stop();
        }
        // The workers are gone, so nothing writes registry state any more:
        // the session-end durable point (save, then the layer's sync hook).
        self.flush_registry();
        // After the workers are gone. A child that still maps the ring keeps
        // its pages until it exits; the name is what goes.
        #[cfg(unix)]
        if let Some(backing) = self.ring_backing.take() {
            remove_memory_ring(Some(&backing), &self.state_dir.join(RING_FILE));
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Session {
    /// On unix: stop serving, remove the root links `launch` placed in a
    /// prefix (only those still symlinks to what this session linked), and
    /// delete the anonymous prefix this session booted — after stopping its
    /// `wineserver`, which would otherwise write the registry back into it. A named prefix
    /// ([`Session::set_prefix_name`]) is persistent and left alone. Best
    /// effort — a destructor has nowhere to report a failure.
    fn drop(&mut self) {
        // Unix flushes inside `stop_serve`, below.
        #[cfg(windows)]
        self.stop_serve();
        #[cfg(unix)]
        {
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
}

/// An anonymous Wine prefix a session booted: its id under `home`'s
/// `sessions/`, and the runtime whose `wineserver` serves it.
#[cfg(unix)]
struct AnonPrefix {
    id: String,
    home: ProtonRoot,
    runtime: PathBuf,
    /// Where the prefix is under `home` for the session's `PrefixInit`.
    prefix_dir: PathBuf,
}

/// What a waited-on Proton [`Session::launch`] returns when
/// [`Session::stop_launch`] ended it: 128 + `SIGKILL`, the shell's
/// convention. Stopping kills the prefix's processes, and `wine` does not
/// report that reliably (it can exit 0), so the session says so instead.
pub const STOPPED_EXIT_CODE: i32 = 128 + 9;

/// How a Proton launch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchExit {
    /// The program exited by itself, with this code.
    Exited(i32),
    /// [`LaunchStopper::stop`] (or [`LaunchHandle::stop`]) ended it.
    Stopped,
}

/// A running Proton launch, from [`Session::launch_detached`].
///
/// **Liveness follows the prefix, not the first process.** A launch runs
/// until no Wine process is left in the session's own prefix — its
/// `wineserver` has exited — not merely until the `wine` process running the
/// injector's target exits. A launcher that starts the game and exits
/// (`skse64_loader.exe` starts `SkyrimSE.exe`) therefore stays running for as
/// long as the game does: [`LaunchHandle::try_wait`] and
/// [`LaunchHandle::is_running`] (non-blocking) and [`LaunchHandle::wait`]
/// (blocking) all see the prefix, and [`LaunchHandle::stop`] and
/// [`Session::stop_launch`] still stop it after the launcher is gone. The
/// exit code reported is the launcher's (the injector's target's);
/// [`LaunchExit::Stopped`] when a stop was requested. The prefix is locked to
/// this launch, so nothing else runs there; the prefix going quiet includes
/// `wineserver`'s few seconds of persistence after the last process.
///
/// Holds the prefix's lock until the launch ends. Dropping it while the
/// program runs stops the program.
#[cfg(unix)]
pub struct LaunchHandle {
    child: std::process::Child,
    wine: WineLaunch,
    stopper: LaunchStopper,
    /// `child`'s exit status, once reaped.
    wine_status: Option<std::process::ExitStatus>,
    /// `wineserver -w` for this prefix, started once `child` has exited: the
    /// launch runs until it returns.
    quiet: Option<std::process::Child>,
    /// How the launch ended, once the prefix is quiet.
    outcome: Option<Result<LaunchExit, String>>,
    _prefix_lock: PrefixLock,
}

/// Stops a running Proton launch from any thread: see
/// [`LaunchHandle::stopper`].
#[cfg(unix)]
#[derive(Clone, Debug)]
pub struct LaunchStopper(Arc<StopInner>);

#[cfg(unix)]
#[derive(Debug)]
struct StopInner {
    prefix: Prefix,
    runtime: PathBuf,
    requested: std::sync::atomic::AtomicBool,
    /// Set once this launch has been reaped — by [`LaunchHandle::conclude`]
    /// or its `Drop` — **before** `_prefix_lock` releases. A stopper kept
    /// past that point (the caller's own, or one handed out and forgotten)
    /// must not run `wineserver -k` on the prefix: once the lock is free, a
    /// later launch can be running there instead, and `wineserver -k` cannot
    /// tell the two apart. [`LaunchStopper::stop`]'s check-then-act and this
    /// flag share one mutex, so the two can never race past each other: if
    /// `stop` gets there first the launch is still this one's and the kill
    /// is real; if reaping gets there first `stop` sees `ended` and no-ops.
    ended: Mutex<bool>,
}

/// How long [`LaunchHandle::stop`] waits for `wine` after stopping the
/// prefix's `wineserver` before killing it.
#[cfg(unix)]
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(unix)]
impl LaunchStopper {
    /// Whether the launch has ended — see [`StopInner::ended`].
    fn has_ended(&self) -> bool {
        self.0.ended.lock().map(|e| *e).unwrap_or(true)
    }

    /// Stops the launch: stops its prefix's `wineserver` (bounded), which ends
    /// every Wine process in the prefix — the program, anything it started,
    /// and the injector. The prefix is locked to this launch, so nothing
    /// else is running there.
    ///
    /// A no-op, `Ok(())`, once the launch has already ended — see
    /// [`StopInner::ended`]. Without that check, a stopper kept past its
    /// launch's life (the fixed window `Session::launch` publishes one in
    /// `self.waiting` for, or simply a clone a caller held onto) could run
    /// `wineserver -k` against whatever the same prefix runs next.
    pub fn stop(&self) -> Result<(), String> {
        let ended = self
            .0
            .ended
            .lock()
            .map_err(|_| "stop: launch-ended lock poisoned".to_string())?;
        if *ended {
            return Ok(());
        }
        self.0.requested.store(true, std::sync::atomic::Ordering::SeqCst);
        let result = self
            .0
            .prefix
            .stop_wineserver(&self.0.runtime)
            .map_err(|e| format!("stop: {e}"));
        drop(ended);
        result
    }

    /// Whether [`LaunchStopper::stop`] has been called.
    pub fn was_stopped(&self) -> bool {
        self.0.requested.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Marks the launch ended — see [`StopInner::ended`]. Idempotent.
    fn mark_ended(&self) {
        if let Ok(mut ended) = self.0.ended.lock() {
            *ended = true;
        }
    }
}

/// Resets [`Session::starting`] to `false` when a
/// [`Session::launch_detached`] call ends, on every path — success or an
/// early `?` return alike. Clears [`Session::stop_pending`] the same way:
/// a `stop_launch` landing while this call is in flight but before it
/// reaches the spawn checkpoint (an early error — a bad image, a prefix
/// that won't `ensure`) would otherwise leave that flag set with nothing
/// left to consume it, and the *next*, unrelated `launch_detached` would
/// spawn its program only to stop it immediately.
#[cfg(unix)]
struct StartingGuard<'a> {
    starting: &'a std::sync::atomic::AtomicBool,
    stop_pending: &'a std::sync::atomic::AtomicBool,
}

#[cfg(unix)]
impl Drop for StartingGuard<'_> {
    fn drop(&mut self) {
        self.starting.store(false, std::sync::atomic::Ordering::SeqCst);
        self.stop_pending.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(unix)]
impl LaunchHandle {
    /// The pid of the `wine` process running `vfs-injector.exe`, which lives
    /// as long as the injector's target does — not necessarily as long as the
    /// launch (see [`LaunchHandle`]). Not the program's own pid.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// What the launch has to say, one line each: what it said before it
    /// started (the Steam client is not running — also written at the top of
    /// [`LaunchOpts::log_file`], or to stderr without one), then, once the
    /// injector has reported, why the Steam helper is not running or that
    /// the Windows artifacts are too old to start it
    /// ([`LaunchHandle::steam_helper_status`]). Call it again after the
    /// program has started for the second part.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = self.wine.notes.clone();
        notes.extend(vfs_proton::steam::helper_note(
            &self.wine.steam,
            &self.steam_helper_status(),
        ));
        notes
    }

    /// What became of Proton's Steam helper
    /// ([`Session::set_steam_helper`]): not asked for, still pending,
    /// started, cleared, not running and why, or not reported by an injector
    /// that predates it. Read from the injector's report beside the ready
    /// file; final once the program has started or `wine` has exited.
    pub fn steam_helper_status(&self) -> vfs_proton::HelperStatus {
        vfs_proton::steam::helper_status(
            &self.wine.steam,
            &self.wine.ready_file,
            self.wine_status.is_some() || self.outcome.is_some(),
        )
    }

    /// Whether the launch asked the injector to start Proton's Steam helper
    /// ([`Session::set_steam_helper`]); [`LaunchHandle::steam_helper_status`]
    /// says whether it did.
    pub fn steam_helper(&self) -> bool {
        matches!(self.wine.steam, SteamSide::Helper(_))
    }

    /// A stopper for this launch, to stop it from another thread while this
    /// handle is being waited on.
    pub fn stopper(&self) -> LaunchStopper {
        self.stopper.clone()
    }

    /// Whether the launch is still running: a Wine process is left in its
    /// prefix. Never blocks.
    pub fn is_running(&mut self) -> bool {
        matches!(self.poll(), Ok(None))
    }

    /// How the launch ended, if it has — see [`LaunchHandle`]. Never blocks.
    pub fn try_wait(&mut self) -> Result<Option<LaunchExit>, String> {
        self.poll()
    }

    /// Waits for the launch to end: for the prefix to be quiet, which is as
    /// long as the program (and anything it started) runs.
    pub fn wait(mut self) -> Result<LaunchExit, String> {
        self.block()
    }

    /// Stops the launch ([`LaunchStopper::stop`]) and waits for it to end,
    /// killing `wine` and the prefix watch if they outlive the prefix's
    /// `wineserver` by [`STOP_WAIT`].
    pub fn stop(mut self) -> Result<LaunchExit, String> {
        let stopped = self.stopper.stop();
        let deadline = std::time::Instant::now() + STOP_WAIT;
        let exit = loop {
            if let Some(exit) = self.poll().transpose() {
                break exit;
            }
            if std::time::Instant::now() >= deadline {
                self.abandon();
                break self.conclude();
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        stopped?;
        exit
    }

    /// One non-blocking step: reap `wine` if it has exited, then watch the
    /// prefix, and conclude once it is quiet.
    fn poll(&mut self) -> Result<Option<LaunchExit>, String> {
        if let Some(outcome) = &self.outcome {
            return outcome.clone().map(Some);
        }
        if self.wine_status.is_none() {
            match self.child.try_wait().map_err(|e| format!("launch: {e}"))? {
                Some(status) => self.wine_status = Some(status),
                None => return Ok(None),
            }
        }
        if !self.prefix_quiet(false)? {
            return Ok(None);
        }
        self.conclude().map(Some)
    }

    /// [`Self::poll`], blocking until the launch has ended.
    fn block(&mut self) -> Result<LaunchExit, String> {
        if let Some(outcome) = &self.outcome {
            return outcome.clone();
        }
        if self.wine_status.is_none() {
            self.wine_status = Some(self.child.wait().map_err(|e| format!("launch: {e}"))?);
        }
        self.prefix_quiet(true)?;
        self.conclude()
    }

    /// Whether no Wine process is left in the prefix, once `wine` itself has
    /// exited: `wineserver -w` for it, started on first call, has returned
    /// (`block`: waited for). A watch that cannot be started counts as quiet
    /// — there is then nothing to observe the prefix with, and holding the
    /// launch open forever would be worse than ending it with `wine`.
    fn prefix_quiet(&mut self, block: bool) -> Result<bool, String> {
        if self.quiet.is_none() {
            match self.stopper.0.prefix.spawn_wineserver_wait(&self.stopper.0.runtime) {
                Ok(watch) => self.quiet = Some(watch),
                Err(_) => return Ok(true),
            }
        }
        let watch = self.quiet.as_mut().expect("started above");
        if block {
            watch.wait().map_err(|e| format!("launch: {e}"))?;
            return Ok(true);
        }
        Ok(watch.try_wait().map_err(|e| format!("launch: {e}"))?.is_some())
    }

    /// Kills and reaps whatever of the launch this handle still has a
    /// process for: `wine`, and the prefix watch.
    fn abandon(&mut self) {
        if self.wine_status.is_none() {
            let _ = self.child.kill();
            self.wine_status = self.child.wait().ok();
        }
        if let Some(watch) = &mut self.quiet {
            let _ = watch.kill();
            let _ = watch.wait();
        }
    }

    /// The prefix is quiet (or abandoned) and `wine` reaped: decide how the
    /// launch ended, record it, and mark it ended — see
    /// [`StopInner::ended`] — before `self` (and so `_prefix_lock`) can drop.
    fn conclude(&mut self) -> Result<LaunchExit, String> {
        let was_stopped = self.stopper.was_stopped();
        self.stopper.mark_ended();
        let outcome = if was_stopped {
            Ok(LaunchExit::Stopped)
        } else {
            match self.wine_status {
                Some(status) => vfs_proton::launch::finish(&self.wine, status)
                    .map(LaunchExit::Exited)
                    .map_err(|e| format!("launch: {e}")),
                None => Err("launch: wine could not be reaped".to_string()),
            }
        };
        self.outcome = Some(outcome.clone());
        outcome
    }
}

#[cfg(unix)]
impl Drop for LaunchHandle {
    /// A handle dropped while its launch runs — `wine`, or anything left in
    /// its prefix after `wine` exited — stops it: nothing could stop it
    /// afterwards, and the prefix lock it held is released here. Marks the
    /// launch ended either way (idempotent if [`Self::conclude`] already
    /// did), before that release — see [`StopInner::ended`].
    fn drop(&mut self) {
        if matches!(self.poll(), Ok(None)) {
            let _ = self.stopper.stop();
            self.abandon();
        }
        self.stopper.mark_ended();
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
const RING_FILE: &str = "ring.bin";

/// Where [`Session::launch`] links the session's directories inside the
/// prefix's `drive_c` — see [`Session::link_into_prefix`].
#[cfg(unix)]
const WINE_LINK_DIR: &str = "vfs-session";

/// Root 0's location inside the prefix when none is declared — the path it has
/// always had, `C:\` + [`WINE_LINK_DIR`] + `\root`, so existing hosts and tests
/// see no change.
#[cfg(unix)]
const DEFAULT_ROOT0_LOCATION: &str = r"C:\vfs-session\root";

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

/// Whether `dir` is on a filesystem whose pages are never written to disk.
///
/// Read from `/proc/self/mountinfo`: the mount whose mount point is the
/// longest prefix of `dir` is the one `dir` is on. A directory this cannot
/// place is reported as not in memory, which only costs the caller the
/// optimisation.
#[cfg(unix)]
fn is_memory_fs(dir: &Path) -> bool {
    let Ok(dir) = dir.canonicalize() else {
        return false;
    };
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    memory_fs_in(&mounts, &dir)
}

/// The parsing half of [`is_memory_fs`], over the text of a `mountinfo`.
#[cfg(unix)]
fn memory_fs_in(mountinfo: &str, dir: &Path) -> bool {
    let mut best: Option<(usize, bool)> = None;
    for line in mountinfo.lines() {
        // `id parent maj:min root MOUNTPOINT opts [optional…] - FSTYPE source superopts`
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let Some(point) = left.split(' ').nth(4) else {
            continue;
        };
        // The kernel writes a space in a path as `\040`.
        let point = point.replace("\\040", " ");
        if !dir.starts_with(&point) {
            continue;
        }
        let fstype = right.split(' ').next().unwrap_or("");
        let in_memory = matches!(fstype, "tmpfs" | "ramfs");
        // Later lines win a tie: a mount over the same point shadows the
        // earlier one.
        if best.is_none_or(|(len, _)| point.len() >= len) {
            best = Some((point.len(), in_memory));
        }
    }
    best.is_some_and(|(_, in_memory)| in_memory)
}

/// Whether `dir` is this user's alone: owned by the user this process runs
/// as, with no access for group or others.
///
/// The effective uid is read as the owner of `/proc/self`, which the kernel
/// reports as exactly that; this crate has no `libc` to ask with.
#[cfg(unix)]
fn is_private_dir(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(d), Ok(me)) = (
        std::fs::symlink_metadata(dir),
        std::fs::metadata("/proc/self"),
    ) else {
        return false;
    };
    d.is_dir() && d.uid() == me.uid() && d.mode() & 0o077 == 0
}

/// Give the ring a home in memory, and make `named` a symlink to it.
///
/// Returns the ring's real path — `$XDG_RUNTIME_DIR/aether-vfs/ring-<id>/` +
/// the same file name as `named`, so the name a Wine child is given does not
/// change — or `None` when the ring should be created at `named` itself:
/// there is no `$XDG_RUNTIME_DIR`, it is not a memory filesystem, it is not
/// private to this user, or anything about setting the file and the link up
/// failed.
///
/// **The directory is checked, not trusted.** `$XDG_RUNTIME_DIR` is the
/// user's own and mode 0700 by specification, but it is only an environment
/// variable: set to `/tmp` (a tmpfs on many systems, and what some sessions
/// without logind do) it is a place where another user can create
/// `aether-vfs` first, own the directory the ring is about to be made in,
/// and swap the file. So both it and `aether-vfs` must be owned by this
/// user with no access for anyone else, and the ring file is created
/// exclusively (never opened if it exists, never through a link) with mode
/// 0600. `/dev/shm` is not considered at all.
///
/// The directory is named for the *canonical* `state_dir` — two processes
/// that spell one state directory differently must share a ring name, and
/// two that spell different ones alike (the same relative path from
/// different working directories) must not. So a session that died without
/// [`Session::stop_serve`] has its file replaced by the next one on the
/// same state directory rather than left to accumulate.
///
/// The file is sparse: its pages are taken from the tmpfs as they are first
/// touched, as they were from the disk before. A runtime directory with less
/// than a ring's worth of room left (64 MiB; the usual quota is a tenth of
/// RAM) can therefore fault the director or the game when it fills. Reserving
/// the pages up front needs `posix_fallocate`, which this crate cannot call
/// without `libc`; not done.
#[cfg(unix)]
fn ring_in_memory(state_dir: &Path, named: &Path) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    if !runtime.is_absolute() || !is_memory_fs(&runtime) || !is_private_dir(&runtime) {
        return None;
    }
    let ours = runtime.join("aether-vfs");
    let _ = std::fs::DirBuilder::new().mode(0o700).create(&ours);
    if !is_private_dir(&ours) {
        return None;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    state_dir.canonicalize().ok()?.hash(&mut h);
    let dir = ours.join(format!("ring-{:016x}", h.finish()));
    let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
    if !is_private_dir(&dir) {
        return None;
    }
    let file = dir.join(named.file_name()?);
    // The same fresh-inode rule as the file in `state_dir`: see `serve`.
    let _ = std::fs::remove_file(&file);
    let made = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)
        .is_ok()
        && std::os::unix::fs::symlink(&file, named).is_ok();
    if !made {
        remove_memory_ring(Some(&file), named);
        return None;
    }
    Some(file)
}

/// Undo [`ring_in_memory`]: the file, its directory, and the link at `named`
/// if it still points at that file. Best effort.
#[cfg(unix)]
fn remove_memory_ring(backing: Option<&Path>, named: &Path) {
    let Some(file) = backing else {
        return;
    };
    let _ = std::fs::remove_file(file);
    if let Some(dir) = file.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    if std::fs::read_link(named).is_ok_and(|to| to == file) {
        let _ = std::fs::remove_file(named);
    }
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

#[cfg(all(test, unix))]
mod ring_location_tests {
    use super::*;

    /// A trimmed `mountinfo`: a disk root, a tmpfs under it, a disk mount
    /// under that tmpfs, and a mount point with a space in its name.
    const MOUNTS: &str = "\
25 1 0:23 / / rw,relatime shared:1 - btrfs /dev/mapper/root rw,compress=zstd:3
30 25 0:27 / /run rw,nosuid shared:2 - tmpfs tmpfs rw,mode=755
61 30 0:52 / /run/user/1000 rw,nosuid,nodev shared:9 - tmpfs tmpfs rw,size=9646860k
70 61 8:1 / /run/user/1000/disk rw - ext4 /dev/sda1 rw
80 25 0:60 / /mnt/my\\040ram rw - ramfs none rw
";

    #[test]
    fn a_directory_is_in_memory_only_if_its_innermost_mount_is() {
        let on = |p: &str| memory_fs_in(MOUNTS, Path::new(p));
        assert!(on("/run/user/1000"));
        assert!(on("/run/user/1000/aether-vfs/ring-1"));
        assert!(on("/run/lock"));
        assert!(on("/mnt/my ram/x"), "an escaped space in a mount point");
        assert!(!on("/home/me/state"), "the disk root");
        assert!(
            !on("/run/user/1000/disk/x"),
            "a disk mounted inside a tmpfs is a disk"
        );
        // A sibling whose name merely starts the same is not under that
        // mount: it is on the tmpfs around it.
        assert!(on("/run/user/1000/disk2/x"));
        assert!(!memory_fs_in("", Path::new("/run")), "no table, no claim");
    }

    /// A runtime directory is used only if it is this user's and nobody
    /// else's. A directory anyone can write to — what `XDG_RUNTIME_DIR=/tmp`
    /// would be — is refused, and so is one with any group or other access.
    #[test]
    fn only_a_directory_private_to_this_user_may_hold_the_ring() {
        use std::os::unix::fs::PermissionsExt;
        let base = crate::test_scratch::scratch_created("ringpriv");
        let set = |mode: u32| {
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        set(0o700);
        assert!(is_private_dir(&base));
        for open in [0o1777, 0o755, 0o750, 0o701] {
            set(open);
            assert!(!is_private_dir(&base), "mode {open:o} is not private");
        }
        set(0o700);
        // Not ours: the root directory belongs to root (unless we are root).
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata("/proc/self").unwrap().uid() != 0 {
            assert!(!is_private_dir(Path::new("/")));
        }
        // A link to a private directory is not a directory of ours.
        let link = base.join("link");
        std::os::unix::fs::symlink(&base, &link).unwrap();
        assert!(!is_private_dir(&link));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The ring's directory is named for where the state directory *is*, not
    /// for how its path was spelled: one directory spelled two ways gets one
    /// ring name, so a second session there replaces the first's file.
    #[test]
    fn the_ring_directory_is_named_for_the_canonical_state_directory() {
        let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) else {
            return;
        };
        if !is_memory_fs(&runtime) || !is_private_dir(&runtime) {
            return;
        }
        let base = crate::test_scratch::scratch_dir("ringname");
        let state = base.join("state");
        std::fs::create_dir_all(state.join("sub")).unwrap();
        let direct = ring_in_memory(&state, &state.join("a.bin")).unwrap();
        let roundabout =
            ring_in_memory(&state.join("sub").join(".."), &state.join("b.bin")).unwrap();
        assert_eq!(direct.parent(), roundabout.parent());
        let other = ring_in_memory(&state.join("sub"), &state.join("c.bin")).unwrap();
        assert_ne!(direct.parent(), other.parent());
        remove_memory_ring(Some(&direct), &state.join("a.bin"));
        remove_memory_ring(Some(&roundabout), &state.join("b.bin"));
        remove_memory_ring(Some(&other), &state.join("c.bin"));
        assert!(!direct.parent().unwrap().exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `serve` on a host with a tmpfs `$XDG_RUNTIME_DIR` (any desktop Linux;
    /// skipped elsewhere): the ring's pages are in memory, the name in
    /// `state_dir` still opens it, and `stop_serve` leaves nothing behind.
    #[test]
    fn the_ring_is_created_in_memory_and_named_in_the_state_dir() {
        let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) else {
            return;
        };
        if !is_memory_fs(&runtime) {
            return;
        }
        let base = crate::test_scratch::scratch_dir("ringloc");
        let mut s = Session::new();
        s.set_root(base.join("root"));
        s.set_overlay(base.join("overlay"));
        s.set_state_dir(base.join("state"));
        s.serve().unwrap();

        let named = base.join("state").join(RING_FILE);
        let real = s.ipc().unwrap().ring_path().unwrap().to_path_buf();
        assert!(
            real.starts_with(&runtime),
            "{} is not in memory",
            real.display()
        );
        assert_eq!(
            real.file_name(),
            named.file_name(),
            "launch names the ring to the child by this file name under the state directory"
        );
        assert_eq!(std::fs::read_link(&named).unwrap(), real);
        // What the child does: open the name in the state directory and find
        // this ring there, whole.
        let len = std::fs::metadata(&named).unwrap().len() as usize;
        assert_eq!(len, s.ipc().unwrap().map_bytes);

        // Serving again while serving changes nothing.
        s.serve().unwrap();
        assert_eq!(s.ipc().unwrap().ring_path().unwrap(), real);

        // Nobody else can read the ring or put another in its place.
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(&real).unwrap().mode() & 0o777, 0o600);
        assert!(is_private_dir(real.parent().unwrap()));
        assert!(is_private_dir(&runtime.join("aether-vfs")));

        s.stop_serve();
        assert!(!real.exists(), "the ring file must not outlive its session");
        assert!(!real.parent().unwrap().exists());
        assert!(std::fs::symlink_metadata(&named).is_err(), "nor its name");

        // And a second serve of the same session gets a ring again.
        s.serve().unwrap();
        assert!(named.exists());
        drop(s);
        assert!(!real.exists(), "dropping the session stops serving");
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod launch_image_tests {
    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unix_default_root_zero_location_is_where_the_session_dir_is_linked() {
        assert_eq!(DEFAULT_ROOT0_LOCATION, format!(r"C:\{WINE_LINK_DIR}\root"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_extra_roots_follow_root_zero_in_declaration_order() {
        let mut s = Session::new();
        s.declare_root(2, r"C:\b");
        s.declare_root(1, r"C:\a");
        let ids: Vec<u32> = s.root_locations().iter().map(|r| r.id).collect();
        assert_eq!(ids, [0, 2, 1]);
        assert_eq!(s.root_backing_dir(2), Some(s.state_dir().join("roots").join("2")));
        assert_eq!(s.root_backing_dir(0).as_deref(), Some(s.virtual_root()));
        assert_eq!(s.root_backing_dir(7), None);
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

    #[test]
    fn io_workers_default_clamp_and_reach_the_ring() {
        let mut s = crate::test_scratch::session_in_scratch("workers");
        assert_eq!(s.io_workers(), 4, "the default is unchanged");
        s.set_io_workers(0);
        assert_eq!(s.io_workers(), 1);
        s.set_io_workers(500);
        assert_eq!(s.io_workers(), 32);
        s.set_io_workers(12);
        s.serve().unwrap();
        assert_eq!(s.ipc().unwrap().worker_count(), 12);
        s.stop_serve();
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
