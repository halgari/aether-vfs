//! Host session: configure mounts + paths, serve IPC, **launch a process** with
//! all NT I/O under the virtual root remapped through this director.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
// Only `launch`'s Windows body has a timeout to express; gated with it so a
// Linux `--tests` check is warning-free.
#[cfg(windows)]
use std::time::Duration;

// `vfs_director::ipc` is portable, and `Session` now uses **both** of its
// halves: the named-section handshake on Windows (`IpcServe::start`, the event
// pair, `write_thin_config`, `apply_env_roots`) and the file-backed ring on
// unix (`IpcServe::start_file_backed`), which is how a shim inside Wine reaches
// a native Linux director. So neither this import nor the `ipc` field below is
// gated; only the two bodies that pick a transport are.
use vfs_director::ipc::IpcServe;
use crate::image::{self, ImageTarget, RootLocation};
use vfs_director::stage::{stage_launch_into, ImageSource, StagedDir};
use vfs_director::{Director, DiskProvider, MountGraph};
// The Proton delivery mechanism: the unix counterpart of the `vfs-inject` +
// `vfs-shim` pair, carrying GE-Proton discovery, the per-session Wine prefix,
// and the injector's positional argv plus the shim's env handshake. Gated in
// the manifest too (`[target.'cfg(unix)'.dependencies]`).
#[cfg(unix)]
use vfs_proton::{
    launch::WineLaunch,
    layout::Root as ProtonRoot,
    prefix::{Prefix, PrefixInit, PrefixLock},
};
use vfs_provider::{
    bad_request, exists, map_io_err, overlay_layer_dir, Access, DirEntry, Provider, RootId, Stat,
    OPEN_READ,
};

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
/// hosts this crate exists for are multi-threaded by construction: a Node
/// addon has libuv's threadpool and V8 alongside it, an Electron main process
/// more still. Serializing our own writers is the floor, not the fix; the fix
/// is to stop touching process env at all and hand `CreateProcessW` an
/// explicit environment block built for the child (see [`Session::launch`]).
///
/// Windows-only: `serve`'s `apply_env_roots` and `launch`'s `opts.env`
/// save/set/restore. The unix bodies never write process env: a Wine child's
/// environment block is built by `vfs_proton::launch::launch_env`, `opts.env`
/// included.
#[cfg(windows)]
static LAUNCH_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Options for [`Session::launch`].
#[derive(Clone, Debug)]
pub struct LaunchOpts {
    /// Path to the image to launch.
    ///
    /// Resolved against the session's root **locations**
    /// ([`Session::root_locations`], [`crate::image::classify_image`]): a
    /// **relative** name is shorthand for root 0; an **absolute** path inside
    /// a root's location is that root's vpath — a real file there is launched
    /// as is, one only root 0's provider graph serves is staged first (see
    /// [`Session::launch`]); an absolute path outside every root is a real
    /// program, launched as given. Anything else is refused by name rather
    /// than handed to `CreateProcess`.
    pub image: String,
    pub args: Vec<String>,
    /// Wait for process exit (false = detach; session must stay alive).
    ///
    /// On the Proton path a detached launch is held by the session: stop it
    /// with [`Session::stop_launch`] (dropping the session also stops it). A
    /// host that wants the handle itself calls [`Session::launch_detached`].
    pub wait: bool,
    /// Extra images to stage beside a graph-resolved `image`, by vpath, each
    /// with its own PE import closure. Ignored when nothing is staged.
    ///
    /// A launcher that spawns the real game (SKSE's `skse64_loader.exe` starts
    /// `SkyrimSE.exe`) needs its target on disk beside it: the child's own
    /// `CreateProcess` needs a real image just as much as the first one did,
    /// and nothing intercepts it. Naming it here is how a host says so.
    pub stage_also: Vec<String>,
    /// Real-disk directories searched for imports the provider graph does not
    /// carry. Ignored when nothing is staged.
    ///
    /// Redistributables (`d3dx9_42.dll` and friends) are static imports of the
    /// game but ship with a runtime rather than in the game archive, so
    /// without a fallback the loader fails them during process init — before
    /// any hook of ours exists to help.
    pub stage_fallback_dirs: Vec<PathBuf>,
    /// Absolute paths to `vfs_shim_dll.dll` and `vfs_payload.dll`.
    ///
    /// Left `None`, they are searched for **next to `std::env::current_exe()`**
    /// — and that is only the right answer when the host process *is* one of
    /// this workspace's binaries. For a language binding it is not: inside a
    /// Node addon `current_exe()` is `node.exe`, wherever the user's Node
    /// happens to be installed, and inside a Python extension it is
    /// `python.exe`. The DLLs live beside the addon, which nothing here can
    /// find from the executable.
    ///
    /// **So for any embedding host these are mandatory, not optional.** A
    /// binding should resolve them from its own module path (Node:
    /// `__dirname`) and set both. The symptom otherwise is
    /// "`vfs_shim_dll.dll` not found" from a host that shipped the DLL, with
    /// nothing pointing at why the search looked where it did.
    ///
    /// **On the Proton path these two fields answer for three files.** A Wine
    /// launch also needs `vfs-injector.exe`, and it has no field of its own:
    /// it is looked for **beside `shim_dll`** when that is set, and otherwise
    /// beside `current_exe()` — the one directory `cargo build` puts all three
    /// in. On Linux they are a separate Windows cross-build
    /// (`bin/build-windows`), so a missing one is reported by name (see `locate_wine_artifacts`) rather than surfacing as
    /// a path error out of `wine`.
    pub shim_dll: Option<String>,
    pub payload_dll: Option<String>,
    /// Extra environment variables for the child.
    ///
    /// **Proton path: child-only.** They go into the environment block the
    /// `wine` child is spawned with and nowhere else; `WINEDLLOVERRIDES` is
    /// merged with the launch's own (the caller wins per DLL,
    /// `vfs_proton::launch::merge_dll_overrides`), `WINEDEBUG` replaces the
    /// default `-all`, and a name the launch's handshake uses is refused.
    ///
    /// **Windows: not child-only.** `CreateProcessW` is called with a null environment
    /// block — inheritance *is* the mechanism — so [`Session::launch`] writes
    /// each one into **this process's** environment with `std::env::set_var`,
    /// launches, and restores the previous value. [`LAUNCH_ENV_LOCK`] serializes
    /// that against every other env write this crate performs, so two sessions
    /// cannot interleave; it cannot serialize a host's *own* threads, and
    /// `set_var` in a multi-threaded process races anything else reading the
    /// environment. See [`Session::launch`]'s "Process-global environment"
    /// section for the costed fix.
    pub env: BTreeMap<String, String>,
    /// **Proton path only**: the working directory the program starts in, as
    /// it sees it — a `C:\…` path, or a path relative to root 0's location.
    /// `None` is the image's own directory. On Windows the child starts in
    /// root 0's directory, as before, whatever this says.
    pub cwd: Option<String>,
    /// How long the injector waits for the shim to report ready. `None`:
    /// `VFS_READY_TIMEOUT_SECS` from this process's environment, else 180 s.
    pub ready_timeout: Option<std::time::Duration>,
}

impl Default for LaunchOpts {
    fn default() -> Self {
        LaunchOpts {
            // Deliberately empty rather than a plausible-looking game exe.
            // This used to default to `"SkyrimSE.exe"`, which is both
            // scenario-specific in a general API and the exact relative-image
            // case that cannot work (see the field's doc): a host that wrote
            // `..Default::default()` and forgot `image` got a launch attempt
            // for a file nobody named. `launch` refuses an empty image by
            // name instead.
            image: String::new(),
            args: Vec::new(),
            wait: true,
            stage_also: Vec::new(),
            stage_fallback_dirs: Vec::new(),
            shim_dll: None,
            payload_dll: None,
            env: BTreeMap::new(),
            cwd: None,
            ready_timeout: None,
        }
    }
}

/// What [`Session::stage_launch`] writes to disk, beyond the image itself.
///
/// The staging *directory* and its tag are deliberately not here: they are the
/// session's (`state_dir/stage`, one tag per staged launch), because the
/// session is also what has to hold the resulting [`StagedDir`] alive and mount
/// it back into the graph. A caller choosing its own directory could hand the
/// same one to two sessions.
pub struct StageOpts<'a> {
    /// The image's vpath in root 0's provider graph.
    pub exe_vpath: &'a str,
    /// Additional images to stage into the same directory, each with its own
    /// import closure — see [`LaunchOpts::stage_also`].
    pub also: &'a [&'a str],
    /// Real-disk fallbacks for imports the graph does not carry — see
    /// [`LaunchOpts::stage_fallback_dirs`].
    pub fallback_dirs: &'a [PathBuf],
}

/// Reads whole files out of a session's own composed graph, for
/// [`vfs_director::stage`]. Root 0: staging always concerns the launched
/// image, which lives in the game-directory root.
/// The sole constructor is `launch`'s staging step, on both targets.
/// `stage_launch` stays portable — it takes any `&dyn ImageSource` a host
/// supplies.
struct KernelSource(Arc<Director>);

impl ImageSource for KernelSource {
    fn read(&self, vpath: &str) -> Option<Vec<u8>> {
        let (fh, size, is_dir) = self.0.open(RootId::DEFAULT, vpath, OPEN_READ).ok()?;
        if is_dir {
            let _ = self.0.close(fh);
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        let mut off = 0usize;
        while off < buf.len() {
            match self.0.read(fh, off as u64, &mut buf[off..]) {
                Ok(0) => break,
                Ok(n) => off += n,
                Err(_) => {
                    let _ = self.0.close(fh);
                    return None;
                }
            }
        }
        let _ = self.0.close(fh);
        buf.truncate(off);
        Some(buf)
    }
}

/// Build the single provider one root serves: its sibling mounts as a
/// [`MountGraph`], with the writable layer (if any) composed **over** the
/// whole graph as an [`vfs_compose::OverlayProvider`] upper.
///
/// The one place in the workspace that turns "these sources, that write
/// layer" into a provider. Every surface funnels through it —
/// [`Session::mount`], the daemon's `SessionRegistry`, and the config →
/// graph builder — because the two halves compose in a way neither
/// `MountGraph` nor `stack_layers` can express: an overlay upper is what
/// makes a write to content only a read-only source holds **copy up** rather
/// than fail. A surface that composes its own graph instead gets a session
/// that reads correctly and cannot be written to, which is how the daemon
/// surface lost copy-on-write while the harness kept it (gate 4, Task 6b).
///
/// `ST_BAD_REQUEST` if the upper is not `Access::ReadWrite`, if any mount
/// declares `Access::SeqRead` (see [`reject_sequential`]), or if a mount prefix
/// does not normalize.
pub fn compose_root(
    mounts: Vec<(String, Arc<dyn Provider>)>,
    write_layer: Option<Arc<dyn Provider>>,
) -> Result<Arc<dyn Provider>, i32> {
    // The funnel's own copy of the gate. `Session::mount_at` and
    // `Session::set_root_mounts` both check before they record, so they never
    // reach here with one; this catches the **third** route, which does not go
    // through `Session` at all: `compose_root` is public and re-exported, and
    // both `vfs-directord`'s `SessionRegistry::compose` and `skyrim-live` call
    // it directly and hand the result to `Director::mount`.
    reject_sequential(mounts.iter().map(|(_, p)| p))?;
    let graph: Arc<dyn Provider> = Arc::new(MountGraph::new(mounts)?);
    match write_layer {
        Some(upper) => Ok(Arc::new(
            vfs_compose::OverlayProvider::from_arcs(graph, upper)
                .map_err(|_| bad_request())?,
        )),
        None => Ok(graph),
    }
}

/// Refuse any provider declaring `Access::SeqRead`, with `ST_BAD_REQUEST`.
///
/// Spec §6's mount-time flag table calls an unwrapped `SeqRead` provider a
/// **hard error**, and it is one: the director's read path is
/// `read_at(handle, offset, buf)`, which a forward-only provider answers
/// `ST_NOT_SUPPORTED` to. Such a mount composes cleanly and serves `getattr` and
/// `readdir` correctly, then fails every actual read — inside an injected
/// process, where the symptom is a game that will not load and the cause is
/// nowhere near it. [`crate::SeekableProvider`] is what a caller wraps it in.
///
/// **One function because there is more than one way in, and the check has to
/// mean the same thing through all of them.** It previously lived inline in
/// `Session::mount_at` only, so `Session::set_root_mounts` accepted what
/// `mount_at` refused — and `set_root_mounts` is the path
/// `vfs-directord`'s `SessionRegistry::add_source` takes for *every* source, so
/// the daemon had no gate at all. Callers: [`Session::mount_at`],
/// [`Session::set_root_mounts`], and [`compose_root`].
fn reject_sequential<'a>(
    providers: impl IntoIterator<Item = &'a Arc<dyn Provider>>,
) -> Result<(), i32> {
    for p in providers {
        if p.capabilities().access == vfs_provider::Access::SeqRead {
            return Err(bad_request());
        }
    }
    Ok(())
}

/// Everything one root composes into, before it becomes the single provider
/// `Director` holds for that root.
///
/// The two halves are **not** interchangeable, and that distinction is the
/// whole point of this type: `mounts` are siblings (a `MountGraph` routes a
/// path to whichever of them owns it, later wins), while `write_layer` sits
/// *above* all of them as an overlay upper, which is what makes copy-on-write
/// possible — see [`Session::set_write_layer`].
#[derive(Default, Clone)]
struct RootComposition {
    /// The staged launch directory, if this root has one — see
    /// [`Session::stage_launch`].
    ///
    /// **A separate slot, composed below `mounts`, and that is the whole
    /// point of it.** Staging is a point-in-time copy of what the graph
    /// already said, written out only because `CreateProcess` needs a real
    /// file; it must lose to curated content on every path both serve, and it
    /// must survive a host rebuilding `mounts` wholesale via
    /// [`Session::set_root_mounts`]. Keeping it out of `mounts` is what buys
    /// both: [`Session::recompose`] always puts it first in the `MountGraph`,
    /// and a `MountGraph` resolves by walking its mounts in **reverse**, so
    /// first means last-tried means lowest precedence.
    ///
    /// The daemon expressed the same rule as `STAGING_LAYER = i32::MIN` inside
    /// a `stack_layers` stack, where ascending layer order makes the first
    /// entry the bottom. The two orderings are opposite, which is exactly why
    /// this is a named slot rather than "just mount it and rely on ordering":
    /// relocating that code as a plain `mount_at` inverts it, and the symptom
    /// — a stale staged copy shadowing curated content — is a silent wrong
    /// answer, not a failure.
    staging: Option<Arc<dyn Provider>>,
    /// Every `(prefix, provider)` accumulated for this root, in registration
    /// order (later wins on an overlapping path).
    mounts: Vec<(String, Arc<dyn Provider>)>,
    /// The writable upper this root's writes copy up into, if one is set.
    write_layer: Option<Arc<dyn Provider>>,
}

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

    /// The declared roots beyond root 0, as `apply_env_roots` wants them.
    ///
    /// No `id != 0` filter: [`Session::declare_root`] routes id 0 to
    /// `virtual_root`, which `apply_env_roots` is handed separately, so
    /// nothing here can be root 0. The filter that used to live here was the
    /// mechanism by which a `declare_root(0, …)` was silently discarded —
    /// dropping it keeps the invariant in one place, where it is enforced
    /// rather than compensated for.
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

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Accumulates: later mounts override earlier for the same path, exactly
    /// as `Director`'s own mount list used to. Each call recomposes the full
    /// accumulated list into one `MountGraph` and replaces `RootId::DEFAULT`'s
    /// provider wholesale, since `Director` holds only one provider per root.
    ///
    /// Root 0's convenience form of [`Session::mount_at`].
    pub fn mount(&self, prefix: &str, backend: Arc<dyn Provider>) -> Result<(), i32> {
        self.mount_at(RootId::DEFAULT, prefix, backend)
    }

    /// [`Session::mount`] for a specific root. Appends one mount to `root`'s
    /// accumulated list and recomposes that root; every other root is
    /// untouched.
    ///
    /// **`ST_BAD_REQUEST` for a provider declaring `Access::SeqRead`** — see
    /// [`reject_sequential`], which is the same gate
    /// [`Session::set_root_mounts`] and [`compose_root`] apply.
    ///
    /// Checked here rather than in each host: the binding that has a friendly
    /// message for it is not the only surface that can reach `mount_at`.
    pub fn mount_at(
        &self,
        root: RootId,
        prefix: &str,
        backend: Arc<dyn Provider>,
    ) -> Result<(), i32> {
        reject_sequential([&backend])?;
        {
            let mut roots = self.roots.lock().map_err(|_| map_io_err())?;
            self.claim(&mut roots, root)?
                .mounts
                .push((prefix.to_string(), backend));
        }
        self.recompose(root)
    }

    /// Begin (or continue) composing `root`, refusing to take over a root
    /// something mounted on `Director` **directly**.
    ///
    /// A root this session has never composed, which the director already
    /// serves, belongs to a caller that built its own provider — the shape
    /// `Director::mount`'s doc describes, e.g. `skyrim-live`'s counter-wrapped
    /// root 1. Composing it here would rebuild it from this session's own
    /// (empty) inputs and replace that provider wholesale: the counters, the
    /// overlay, or both would vanish, silently, with reads still working
    /// against the wrong graph. `ST_EXISTS` says so instead; a caller that
    /// really means to take the root over unmounts it first.
    ///
    /// Roots this session already composes pass straight through — this is a
    /// check about *ownership*, not about re-composition.
    fn claim<'m>(
        &self,
        roots: &'m mut BTreeMap<u32, RootComposition>,
        root: RootId,
    ) -> Result<&'m mut RootComposition, i32> {
        if !roots.contains_key(&root.0) && self.kernel.serves(root)? {
            return Err(exists());
        }
        Ok(roots.entry(root.0).or_default())
    }

    /// Replace `root`'s **entire** sibling-mount list, keeping its write
    /// layer, and recompose.
    ///
    /// For a host that keeps its own record of what a root serves and rebuilds
    /// the list from scratch whenever it changes — `SessionRegistry`, which
    /// re-derives a root's layer stack on every `add_source`. Such a host must
    /// not compose the result itself and hand it to `kernel().mount`: doing so
    /// replaces the root's provider with one that has no knowledge of the
    /// write layer, silently removing copy-on-write. Going through here keeps
    /// the two halves composed by the same code path [`Session::mount`] uses.
    ///
    /// **`ST_BAD_REQUEST` for any mount declaring `Access::SeqRead`**, the same
    /// gate [`Session::mount_at`] applies — see [`reject_sequential`] for why
    /// the two agreeing is not cosmetic.
    ///
    /// Validated **before** the list is recorded, for the reason
    /// [`Session::set_write_layer_at`] gives: a rejected call must leave the
    /// session exactly as it was. Recording first and letting
    /// [`Session::recompose`] refuse would park a list that cannot compose,
    /// making every later `mount_at` on this root fail too.
    pub fn set_root_mounts(
        &self,
        root: RootId,
        mounts: crate::RootMounts,
    ) -> Result<(), i32> {
        reject_sequential(mounts.iter().map(|(_, p)| p))?;
        {
            let mut roots = self.roots.lock().map_err(|_| map_io_err())?;
            self.claim(&mut roots, root)?.mounts = mounts;
        }
        self.recompose(root)
    }

    /// Declare the layer root 0's **writes** land in, composed as an
    /// [`vfs_compose::OverlayProvider`] upper over everything [`Session::mount`]
    /// has accumulated. Replaces any previously set write layer; takes effect
    /// immediately and is re-applied by every later `mount`.
    ///
    /// **This is what makes copy-on-write work, and mounting the same
    /// provider as an ordinary sibling layer does not.** A `MountGraph` (and
    /// `LayeredProvider` likewise) can only *route* a write to whichever
    /// mount is willing to take it; neither can seed the destination from a
    /// lower layer first. So with the writable directory mounted as a sibling
    /// above a read-only archive, an in-place edit of archive content — the
    /// `fopen(..., "r+b")` / `CreateFile(OPEN_EXISTING, GENERIC_WRITE)` that
    /// every mod tool does — finds no writable mount holding the file and
    /// fails, either `ST_READ_ONLY` (the archive owns the path) or
    /// `ST_NOT_FOUND` (nothing writable has it). Copy-on-write over read-only
    /// layered content is the core function of a mod-manager VFS, so the
    /// composition has to be an overlay, not a sibling.
    ///
    /// The upper must declare `Access::ReadWrite`; anything else is refused
    /// here (`ST_BAD_REQUEST`) rather than at the first write.
    ///
    /// Root 0's convenience form of [`Session::set_write_layer_at`].
    pub fn set_write_layer(&self, upper: Arc<dyn Provider>) -> Result<(), i32> {
        self.set_write_layer_at(RootId::DEFAULT, upper)
    }

    /// [`Session::set_write_layer`] for a specific root. Each root has its own
    /// write layer: a session may copy up game-directory writes into one
    /// location and a second root's writes into another, or give one root a
    /// write layer and leave the rest read-only.
    ///
    /// The upper is validated **before** it is recorded, so a rejected layer
    /// leaves the session exactly as it was rather than parking an unusable
    /// provider that would make every later `mount` on this root fail too.
    ///
    /// **Never wrap the upper in [`crate::Storage::cached`].** A host is
    /// expected to put slow sources behind the cache and it is natural to do
    /// that uniformly, in one loop, over everything it mounts. The write
    /// layer is the one provider in the graph whose bytes change underneath
    /// the director: a cached read of a file that was just copied up would
    /// serve the pre-write content. (`Storage::cached` returns a mutable
    /// provider unchanged, so a writable upper is not cached in practice; a
    /// named layer from [`crate::Storage::layer`] is the upper as it is.)
    /// `vfs-directord` passes every source through `Storage::cached`, which
    /// wraps only the immutable, slow ones, and never the write layer.
    pub fn set_write_layer_at(&self, root: RootId, upper: Arc<dyn Provider>) -> Result<(), i32> {
        if upper.capabilities().access != Access::ReadWrite {
            return Err(bad_request());
        }
        {
            let mut roots = self.roots.lock().map_err(|_| map_io_err())?;
            self.claim(&mut roots, root)?.write_layer = Some(upper);
        }
        self.recompose(root)
    }

    /// Rebuild `root`'s single provider from its accumulated mounts plus its
    /// optional write layer, and replace whatever `Director` currently serves
    /// for it — `Director` holds exactly one provider per root, so there is no
    /// incremental mount to append to.
    fn recompose(&self, root: RootId) -> Result<(), i32> {
        let composition = self
            .roots
            .lock()
            .map_err(|_| map_io_err())?
            .get(&root.0)
            .cloned()
            .unwrap_or_default();
        // Staging **first**, and this line is the whole precedence guarantee:
        // a `MountGraph` resolves by walking its mounts in reverse, so the
        // first entry is the last one tried and therefore the one that only
        // answers for paths nothing else serves. Appending it instead — the
        // shape `mount_at` would produce — inverts that and lets a
        // point-in-time staged copy shadow curated content. See
        // [`RootComposition::staging`].
        let mut mounts: Vec<(String, Arc<dyn Provider>)> =
            Vec::with_capacity(composition.mounts.len() + 1);
        mounts.extend(composition.staging.map(|p| (String::new(), p)));
        mounts.extend(composition.mounts);
        let composed = compose_root(mounts, composition.write_layer)?;
        self.kernel.mount(root, composed)
    }

    /// Stage `opts.exe_vpath` out of this session's provider graph onto real
    /// disk, with its PE import closure, and mount the staging directory back
    /// into the graph **underneath** everything else. Returns the staged
    /// image's absolute path — what `CreateProcess` needs.
    ///
    /// [`Session::launch`] calls this for you when a relative image is graph
    /// content; call it directly only when you need to seed staging from
    /// something that is *not* this session's graph, or to stage extra images
    /// before a launch.
    ///
    /// Three things happen that a host would otherwise have to know to do:
    ///
    /// * The bytes land in `state_dir/stage`, under a per-launch tag, and the
    ///   resulting [`StagedDir`] is **held by the session** — its `Drop`
    ///   removes the directory, and Windows keeps the image mapped for as long
    ///   as the child runs, so a host-held handle is a race waiting to be lost.
    /// * The staging directory is mounted back, so the same file is answerable
    ///   through `getattr`/`open` at its vpath afterwards and not merely
    ///   reachable by the literal path `CreateProcess` used. Once the managed
    ///   root is fully virtual, a real file under it that no provider serves is
    ///   invisible.
    /// * It is mounted **below** the host's own mounts. See
    ///   [`RootComposition::staging`] — a staged copy outranking curated
    ///   content is a silent wrong answer on exactly the paths staging touches.
    ///
    /// Staging again replaces the previous directory (and deletes it), the same
    /// way a relaunch did in the daemon.
    pub fn stage_launch(
        &self,
        source: &dyn ImageSource,
        opts: &StageOpts,
    ) -> Result<PathBuf, String> {
        // Into the virtual root, at the image's own vpath — not a sibling
        // staging directory. A staged EXE outside the root drags everything
        // the process resolves relative to its own module path out of the
        // VFS with it, which is what stopped Cyberpunk 2077 and Stardew
        // Valley booting while Skyrim (EXE at the root, content found via
        // cwd) was unaffected.
        let staged = stage_launch_into(
            source,
            opts.exe_vpath,
            opts.also,
            &self.virtual_root,
            opts.fallback_dirs,
        )?;
        let disk: Arc<dyn Provider> = Arc::new(DiskProvider::new(staged.dir()));
        {
            let mut roots = self
                .roots
                .lock()
                .map_err(|_| "session roots lock poisoned".to_string())?;
            self.claim(&mut roots, RootId::DEFAULT)
                .map_err(|st| format!("mount staging: status {st}"))?
                .staging = Some(disk);
        }
        self.recompose(RootId::DEFAULT)
            .map_err(|st| format!("mount staging: status {st}"))?;

        let exe = staged.exe().to_path_buf();
        *self
            .staged
            .lock()
            .map_err(|_| "staged-dir lock poisoned".to_string())? = Some(staged);
        Ok(exe)
    }

    /// Drop all of root 0's mounts before rebuilding composition. Its write
    /// layer, if any, is dropped with them — it is part of the same
    /// composition. **Other roots are untouched**, which is why this is
    /// spelled as root 0's form of [`Session::clear_root`] rather than left
    /// looking like it clears the session: a session is multi-root now, and
    /// "clear the mounts" would be a lie about the other roots.
    pub fn clear_mounts(&self) -> Result<(), i32> {
        self.clear_root(RootId::DEFAULT)
    }

    /// Forget everything this session composes for `root` — mounts and write
    /// layer together — and stop serving it. Also the way to hand a root back
    /// so something else can mount it directly (see [`Session::mount_at`]'s
    /// ownership check).
    ///
    /// **No production caller, and neither has [`Session::clear_mounts`].**
    /// The only non-test reference to either is `clear_mounts` delegating
    /// here. Kept deliberately, not overlooked: `Session` is this crate's
    /// public composition API, `mount_at`'s ownership check documents this as
    /// the way out of it, and "stop serving a root" is not something a host
    /// should have to reach past the API to do. Do not read the absence of
    /// callers as evidence the operation is unnecessary — read it as this
    /// project not yet having a host that tears a root down mid-session.
    pub fn clear_root(&self, root: RootId) -> Result<(), i32> {
        self.roots
            .lock()
            .map_err(|_| map_io_err())?
            .remove(&root.0);
        self.kernel.unmount(root)
    }

    /// Every root this session composes, ascending — whether it got there
    /// through [`Session::mount_at`], [`Session::set_root_mounts`] or
    /// [`Session::set_write_layer_at`].
    ///
    /// The last of those is why this exists rather than callers keeping their
    /// own list: a host that records sources per root (as `SessionRegistry`
    /// does) has no entry for a root that was given *only* a write layer, so
    /// its own bookkeeping cannot enumerate what the session actually serves.
    pub fn composed_roots(&self) -> Vec<RootId> {
        self.roots
            .lock()
            .map(|roots| roots.keys().copied().map(RootId).collect())
            .unwrap_or_default()
    }

    /// Whether `root` has a write layer — i.e. whether a write to content
    /// only a read-only source holds can copy up, or must fail. The daemon
    /// reports this per root when a session is composed, since an absent
    /// write layer is otherwise invisible until the first in-place edit
    /// fails, inside a running game.
    pub fn has_write_layer(&self, root: RootId) -> bool {
        self.roots
            .lock()
            .map(|roots| roots.get(&root.0).is_some_and(|c| c.write_layer.is_some()))
            .unwrap_or(false)
    }

    /// Mount a Stored zip archive as a content backend (later mounts win on conflicts).
    ///
    /// Requires the `zip` feature (on by default).
    #[cfg(feature = "zip")]
    pub fn mount_zip(&self, zip_path: impl AsRef<Path>) -> Result<(), String> {
        let path = zip_path.as_ref();
        let be = vfs_zip::ZipProvider::open(path)
            .map_err(|e| format!("ZipProvider {}: {e:?}", path.display()))?;
        self.mount("", Arc::new(be))
            .map_err(|st| format!("mount zip status {st}"))
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

    /// Every write this host has refused because no `ReadWrite` provider
    /// served that path, as `(path, count)` — spec §7's discovery workflow:
    /// launch, ask what was rejected, add an overlay for those subtrees.
    ///
    /// **Process-wide, despite being a method.** `vfs_director::io_stats`
    /// keeps one global table with no session or root dimension, so with two
    /// live sessions in one host each reports the other's rejections. Left
    /// that way deliberately rather than faked per-session: the counters are
    /// recorded deep in the director's open path, and giving them a session
    /// dimension is a change to that path, not to this accessor. See the
    /// free-function form, [`crate::rejected_writes`].
    pub fn rejected_writes(&self) -> Vec<(String, u64)> {
        vfs_director::io_stats::rejected_writes()
    }

    /// Occasional host-side full-file read (not the primary API).
    ///
    /// Root 0's convenience form of [`Session::read_file_at`].
    pub fn read_file(&self, vpath: &str) -> Result<Vec<u8>, i32> {
        self.read_file_at(RootId::DEFAULT, vpath)
    }

    /// Occasional host-side full-file read out of `root`'s graph.
    ///
    /// This takes a root because the spec's own example needs one: §8 mounts the
    /// INI provider on **root 1** and finishes by reading back what the game
    /// wrote to it. Until this existed, `read_file` hardcoded
    /// [`RootId::DEFAULT`] while [`Director::readdir`] already took a root, so a
    /// host could *list* a second root's graph and never read a byte out of it —
    /// the round trip that `memory()` exists for was reachable only by launching
    /// something and having the child copy the file out to real disk.
    ///
    /// [`Director::readdir`]: vfs_director::Director::readdir
    pub fn read_file_at(&self, root: RootId, vpath: &str) -> Result<Vec<u8>, i32> {
        let (fh, size, is_dir) = self.kernel.open(root, vpath, OPEN_READ)?;
        if is_dir {
            let _ = self.kernel.close(fh);
            return Err(vfs_provider::is_dir());
        }
        let mut buf = vec![0u8; size as usize];
        let mut off = 0usize;
        while off < buf.len() {
            let n = self.kernel.read(fh, off as u64, &mut buf[off..])?;
            if n == 0 {
                break;
            }
            off += n;
        }
        let _ = self.kernel.close(fh);
        buf.truncate(off);
        Ok(buf)
    }

    /// List a directory in `root`'s graph, host-side.
    ///
    /// The companion to [`Session::read_file_at`], and the reason it is here
    /// rather than in each host is that **every host was reaching past the seam
    /// for it**: the Node binding called `session.kernel().readdir(...)` and
    /// `vfs-launch` called `session.kernel()` four times for exactly these two
    /// questions. This crate's own doc says "if a host has to reach past this
    /// crate, the fix belongs here" — so it does.
    ///
    /// It is not a convenience. Two of spec §6's rules are statements about
    /// `readdir` and nothing else can check them from a host: `layered`
    /// **unions** its children's listings with top-wins per name, while
    /// `router`'s listing is **single-dispatch** rather than the union §6
    /// specifies, so a file served by a route is readable by name and absent
    /// from its own directory. A host that cannot list its graph cannot tell
    /// those apart, and the second is a silent wrong answer.
    ///
    /// Drives the graph on the calling thread, like `read_file_at`. For a host
    /// whose provider is serviced by that same thread's event loop that is what
    /// trips the binding's deadlock guard — deliberately, because the failure is
    /// then reported instead of hanging.
    pub fn readdir(&self, root: RootId, vpath: &str) -> Result<Vec<DirEntry>, i32> {
        self.kernel.readdir(root, vpath)
    }

    /// Stat one path in `root`'s graph, host-side. `Ok(None)` is "the graph does
    /// not serve it", which is not an error.
    ///
    /// Same reason as [`Session::readdir`]: it was reached for through
    /// `kernel()` by two hosts. It is the cheapest way to answer the question
    /// this project keeps needing answered — *does my graph actually serve the
    /// path I think it does* — without opening anything, and `vfs-launch` uses
    /// it for precisely that before it stages a launch image.
    pub fn getattr(&self, root: RootId, vpath: &str) -> Result<Option<Stat>, i32> {
        self.kernel.getattr(root, vpath)
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
        if self.ipc.is_some() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.virtual_root)
            .map_err(|e| format!("create root: {e}"))?;
        std::fs::create_dir_all(&self.overlay).map_err(|e| format!("create overlay: {e}"))?;
        std::fs::create_dir_all(&self.state_dir).map_err(|e| format!("create state: {e}"))?;

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
            ipc.apply_env_roots(&root_s, &self.extra_roots_env(), &thin);
        }

        // Minimal shim.cfg (FUSE path is env-driven). The snapshot must still be a
        // valid empty tree: Engine::build rejects zero-length snapshot bytes, which
        // would abort dual-layer bootstrap before hooks install.
        let overlay_s = self.overlay.to_string_lossy().into_owned();
        let snap = empty_tree_snapshot();
        let config_bytes =
            vfs_shim::encode_config_with_overlay(&root_s, &overlay_s, &snap);
        let _ = std::fs::write(self.state_dir.join("shim.cfg"), config_bytes);

        self.ipc = Some(ipc);
        Ok(())
    }

    /// Start the control ring + workers, file-backed, so a shim inside Wine
    /// can remap its I/O to this native director. Idempotent if already
    /// serving.
    ///
    /// Same shape as the Windows body above, with the transport swapped: the
    /// ring is a **real file** at `state_dir/ring.bin` that both sides `mmap`
    /// by path, because a Wine process and a native Linux director share no
    /// named section and no event either could wake the other with (see
    /// `IpcServe::start_file_backed`).
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

        let ring = self.state_dir.join(RING_FILE);
        // Unlinked rather than reused. `FileMapping::create` grows a file but
        // never shrinks one and `ring::init` rewrites the header in place, so a
        // ring left by an earlier run of this session would be re-initialised
        // underneath anything still mapping it. Unlinking gives this director a
        // fresh inode and leaves such a reader on the old one, where it fails
        // visibly instead of racing us for slots.
        let _ = std::fs::remove_file(&ring);
        let ipc = IpcServe::start_file_backed_with_workers(
            Arc::clone(&self.kernel),
            &ring,
            PROTON_PAYLOAD_CAP,
            self.io_workers(),
        )?;

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

    /// Where `opts.image` points, staged if need be: the one resolver both
    /// `launch` bodies share. The three forms are
    /// [`crate::image::classify_image`]'s, against [`Session::root_locations`]:
    ///
    /// - **In a root** at a vpath: a real file in that root's backing
    ///   directory is used as is; failing that, a vpath the root's provider
    ///   graph serves is **staged** into root 0's backing directory (root 0
    ///   only — another root's graph-only image is refused by name); failing
    ///   that, refused.
    /// - **Outside every root**: launched as given. On unix only a `C:\…`
    ///   form can be given — a host path has no drive in the prefix.
    ///
    /// On unix an absolute **host** path is first rewritten to its remainder
    /// under `virtual_root` (an accepted form before roots had locations), or
    /// refused if it is not under it.
    fn resolve_launch_image(&self, opts: &LaunchOpts) -> Result<ResolvedImage, String> {
        #[cfg(unix)]
        let image: String = {
            let host = Path::new(&opts.image);
            if host.is_absolute() {
                let rel = host
                    .strip_prefix(&self.virtual_root)
                    .map_err(|_| no_drive_names(&opts.image))?;
                rel.components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            } else {
                opts.image.clone()
            }
        };
        #[cfg(not(unix))]
        let image: String = opts.image.clone();

        match image::classify_image(&image, &self.root_locations())? {
            ImageTarget::Outside(p) => {
                #[cfg(unix)]
                if !image::is_windows_absolute(&p) {
                    return Err(no_drive_names(&p));
                }
                Ok(ResolvedImage::Outside(p))
            }
            ImageTarget::InRoot { root, vpath } => {
                // A component with a drive (`C:foo.exe`, drive-relative, which
                // `classify_image` calls relative) would make `Path::join`
                // replace the whole base on Windows and escape the root.
                if vpath.split('/').any(|c| c.contains(':')) {
                    return Err(format!(
                        "launch: {:?} has a path component containing ':' (a drive-relative \
                         name); name the image by a plain path inside a root, or give it as \
                         an absolute path",
                        opts.image
                    ));
                }
                let base = self
                    .root_backing_dir(root)
                    .ok_or_else(|| format!("launch: root {root} has no backing directory"))?;
                let host = vpath.split('/').fold(base, |p, c| p.join(c));
                if host.is_file() {
                    return Ok(ResolvedImage::InRoot { root, vpath, host });
                }
                let served = self
                    .kernel
                    .getattr(RootId(root), &vpath)
                    .ok()
                    .flatten()
                    .is_some();
                if !served {
                    return Err(format!(
                        "launch: {:?} resolves to root {root} vpath {vpath:?}, which is neither \
                         a real file at {} nor served by that root, so there is nothing to \
                         stage",
                        opts.image,
                        host.display()
                    ));
                }
                if root != 0 {
                    return Err(format!(
                        "launch: {vpath:?} is served by root {root}'s provider graph but is not \
                         a real file, and staging is root 0 only — put the program in root 0 \
                         or on disk"
                    ));
                }
                // VFS content. Write it (and its import closure) out, mount
                // the staging directory back under the curated graph, and
                // launch the real file that produces.
                let also: Vec<&str> = opts.stage_also.iter().map(String::as_str).collect();
                let host = self
                    .stage_launch(
                        &KernelSource(Arc::clone(&self.kernel)),
                        &StageOpts {
                            exe_vpath: &vpath,
                            also: &also,
                            fallback_dirs: &opts.stage_fallback_dirs,
                        },
                    )
                    .map_err(|e| format!("launch: staging {vpath:?}: {e}"))?;
                Ok(ResolvedImage::InRoot { root, vpath, host })
            }
        }
    }

    /// [`Session::resolve_launch_image`]'s host path, for unit tests of the
    /// in-root forms.
    #[cfg(test)]
    fn resolve_for_test(&self, opts: &LaunchOpts) -> Result<PathBuf, String> {
        match self.resolve_launch_image(opts)? {
            ResolvedImage::InRoot { host, .. } => Ok(host),
            ResolvedImage::Outside(p) => Ok(PathBuf::from(p)),
        }
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
    /// else reading the environment, and a Node or Python binding always has
    /// such threads.
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
        let ipc = self
            .ipc
            .as_ref()
            .ok_or_else(|| "serve() before launch()".to_string())?;

        if opts.image.trim().is_empty() {
            return Err("LaunchOpts.image is empty — name the image to launch".to_string());
        }

        // Root 0 may have been declared after `serve`, which created the
        // managed root it had then; resolving (and staging) needs this one.
        std::fs::create_dir_all(&self.virtual_root)
            .map_err(|e| format!("launch: create root {}: {e}", self.virtual_root.display()))?;
        let target = match self.resolve_launch_image(opts)? {
            ResolvedImage::InRoot { host, .. } => host,
            ResolvedImage::Outside(p) => PathBuf::from(p),
        };
        let config_path = self.state_dir.join("shim.cfg");
        // `serve` wrote `shim.cfg` and the thin config from root 0's location
        // as it was then; root 0 may have been declared since. Rewrite both
        // from the current one so the shim is told the root this child sees.
        let root_s = self.virtual_root.to_string_lossy().into_owned();
        let overlay_s = self.overlay.to_string_lossy().into_owned();
        std::fs::write(
            &config_path,
            vfs_shim::encode_config_with_overlay(&root_s, &overlay_s, &empty_tree_snapshot()),
        )
        .map_err(|e| format!("launch: write {}: {e}", config_path.display()))?;
        let thin = self.state_dir.join("fuse.cfg");
        ipc.write_thin_config(&thin, &root_s)?;
        let ready_path = self.state_dir.join("ready.flag");
        let _ = std::fs::remove_file(&ready_path);

        let (dll, payload) = locate_shim_payload(opts)?;
        // Remote LoadLibrary resolves relative to the *child* cwd (managed root,
        // which is intentionally empty). Always use absolute DLL paths.
        // Strip the `\\?\` verbatim prefix — some LoadLibrary paths reject it.
        let strip_verbatim = |s: String| {
            s.strip_prefix(r"\\?\")
                .map(|t| t.to_string())
                .unwrap_or(s)
        };
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
        ipc.apply_env_roots(&root_s, &self.extra_roots_env(), &thin);

        let mut saved: Vec<(String, Option<String>)> = Vec::with_capacity(opts.env.len());
        for (k, v) in &opts.env {
            saved.push((k.clone(), std::env::var(k).ok()));
            std::env::set_var(k, v);
        }

        let ready_timeout = opts.ready_timeout.unwrap_or_else(|| {
            vfs_env::text(vfs_env::READY_TIMEOUT_SECS)
                .and_then(|s| s.parse().ok())
                .map(Duration::from_secs)
                .unwrap_or(Duration::from_secs(180))
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

        exit.map_err(|e| format!("launch: {e:?}"))
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

/// What [`Session::resolve_launch_image`] resolved an image to.
#[derive(Debug)]
enum ResolvedImage {
    /// Inside `root`'s location at `vpath`; `host` is the real file backing
    /// it — already there, or just staged into root 0's backing directory.
    InRoot {
        // The unix body names the child's image by the root's location and
        // this vpath; the Windows body launches `host` and reads neither.
        #[cfg_attr(not(unix), allow(dead_code))]
        root: u32,
        #[cfg_attr(not(unix), allow(dead_code))]
        vpath: String,
        // The Windows body launches `host`; the unix body names the child's
        // image by the root's location instead, so only tests read it there.
        #[cfg_attr(unix, allow(dead_code))]
        host: PathBuf,
    },
    /// Outside every root: a real program, launched as given.
    Outside(String),
}

/// The refusal for a unix host path no Wine drive names.
#[cfg(unix)]
fn no_drive_names(p: &str) -> String {
    format!(
        "launch: {p} is a host path outside every root, so no drive in this session's Wine \
         prefix names it. Give it as the program sees it (C:\\...) or put it in a root."
    )
}

/// Protocol golden `empty-tree-snapshot`: a single empty root directory.
/// Kept inline so `vfs-director` does not need the vfs-core bridge just for this.
///
/// Portable, with its two helpers below: `shim.cfg` is written by `serve` on
/// Windows and by `launch` on unix — where the `C:\` form of the managed root
/// is not known until a Wine prefix exists — and both need this snapshot.
const EMPTY_TREE_SNAPSHOT_HEX: &str = "\
535346560100000000000000000000008000000000000000010000003000000000000000\
800000000000000080000000000000000000000080000000000000000000000000000000\
800000000000000000000000000000000000000000000000000000000000000000000000\
0000000000000000000000000000000000000000";

fn empty_tree_snapshot() -> Vec<u8> {
    let hex = EMPTY_TREE_SNAPSHOT_HEX.as_bytes();
    debug_assert_eq!(
        hex.len(),
        256,
        "empty-tree golden must be 128 bytes (256 hex chars)"
    );
    let mut out = Vec::with_capacity(hex.len() / 2);
    let mut i = 0;
    while i + 1 < hex.len() {
        let hi = from_hex(hex[i]);
        let lo = from_hex(hex[i + 1]);
        out.push((hi << 4) | lo);
        i += 2;
    }
    out
}

fn from_hex(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

// Only `launch`'s Windows body calls this (it resolves `vfs_inject`'s DLL/
// payload pair), so it is gated alongside it.
#[cfg(windows)]
fn locate_shim_payload(opts: &LaunchOpts) -> Result<(String, String), String> {
    if let (Some(d), Some(p)) = (&opts.shim_dll, &opts.payload_dll) {
        return Ok((d.clone(), p.clone()));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dll = opts
        .shim_dll
        .clone()
        .or_else(|| {
            vfs_inject::find_near(&exe, "vfs_shim_dll.dll")
                .map(|p| p.to_string_lossy().into_owned())
        })
        .ok_or_else(|| "vfs_shim_dll.dll not found (set LaunchOpts.shim_dll)".to_string())?;
    let payload = opts
        .payload_dll
        .clone()
        .or_else(|| vfs_inject::ensure_payload_beside_shim(&dll, None))
        .ok_or_else(|| "vfs_payload.dll not found".to_string())?;
    Ok((dll, payload))
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

/// The three Windows binaries a Proton launch needs, resolved and checked, or
/// a message naming exactly which are missing.
///
/// `vfs-injector.exe`, `vfs_shim_dll.dll` and `vfs_payload.dll` are Windows
/// targets, cross-built separately from the Linux host (`bin/build-windows`,
/// which copies them beside the Linux binaries) — so this resolves what is
/// already there rather than producing anything, and says so in the failure. [`LaunchOpts::shim_dll`] / [`LaunchOpts::payload_dll`]
/// win when set; the documented default location is the directory holding
/// `shim_dll` if only that is set, else the directory holding
/// `current_exe()`. The injector has no `LaunchOpts` field of its own (adding
/// one is a change to a public struct, which this increment does not make), so
/// that same directory is where it is looked for — the one `cargo build` puts
/// all three in.
///
/// All three are checked before any of them is used, and every missing one is
/// listed: a launch that reported them one at a time would cost a Wine
/// round-trip per file.
#[cfg(unix)]
fn locate_wine_artifacts(opts: &LaunchOpts) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let base = match &opts.shim_dll {
        Some(s) => Path::new(s)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
        None => std::env::current_exe()
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
             in {}, or set LaunchOpts.shim_dll and LaunchOpts.payload_dll to where they are \
             (vfs-injector.exe is then looked for beside shim_dll).",
            missing.join(", "),
            base.display()
        ));
    }
    Ok((injector, shim, payload))
}

/// Who owns a root's provider — the session that composes it, or a caller
/// that mounted one on `Director` directly.
///
/// `skyrim-live` mounts root 1 by hand because its counters must wrap the
/// composed provider, which `Session` has no hook for. That is legitimate,
/// and it leaves a hazard pointing the other way: any later `mount_at` /
/// `set_write_layer_at` on that root would recompose it from the session's
/// own empty inputs and drop the hand-mounted provider — counters, overlay
/// and all — while reads kept working against the wrong graph.
#[cfg(test)]
mod root_ownership_tests {
    use super::*;
    use vfs_director::DiskProvider;
    use vfs_provider::ST_EXISTS;

    fn dir(tag: &str, file: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("vfs-own-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join(file), file.as_bytes()).unwrap();
        p
    }

    #[test]
    fn a_hand_mounted_root_is_not_recomposed_away() {
        let hand = dir("hand", "hand.txt");
        let session_layer = dir("sess", "session.txt");

        let s = Session::new();
        s.kernel()
            .mount(RootId(1), Arc::new(DiskProvider::new(&hand)))
            .unwrap();

        // Both mutators must refuse: each one alone would replace root 1's
        // provider with a composition built from nothing.
        assert_eq!(
            s.mount_at(RootId(1), "", Arc::new(DiskProvider::new(&session_layer)))
                .expect_err("composing a hand-mounted root must be refused, not performed"),
            ST_EXISTS
        );
        assert_eq!(
            s.set_write_layer_at(RootId(1), Arc::new(DiskProvider::new(&session_layer)))
                .expect_err("a write layer on a hand-mounted root must be refused too"),
            ST_EXISTS
        );

        // The hand-mounted provider still serves, and the refused mount never
        // took effect — the refusal is not a half-applied change.
        assert!(
            s.kernel().getattr(RootId(1), "hand.txt").unwrap().is_some(),
            "the hand-mounted provider must still be serving root 1"
        );
        assert!(
            s.kernel().getattr(RootId(1), "session.txt").unwrap().is_none(),
            "the refused mount must not be serving anything"
        );

        // Root 0 is unaffected: this is per-root ownership, not a session-wide
        // freeze.
        s.mount("", Arc::new(DiskProvider::new(&session_layer))).unwrap();
        assert!(s.kernel().getattr(RootId::DEFAULT, "session.txt").unwrap().is_some());

        // And the root can be handed over deliberately.
        s.clear_root(RootId(1)).unwrap();
        s.mount_at(RootId(1), "", Arc::new(DiskProvider::new(&session_layer)))
            .expect("an unmounted root may be taken over");
        assert!(s.kernel().getattr(RootId(1), "session.txt").unwrap().is_some());
        assert!(
            s.kernel().getattr(RootId(1), "hand.txt").unwrap_or(None).is_none(),
            "after the handover the hand-mounted provider is gone, as asked for"
        );
    }

    /// A root given only a write layer is still a root this session composes.
    ///
    /// The daemon enumerates roots through this to report whether each can
    /// copy up. Its own per-root bookkeeping is filled in by `add_source`
    /// alone, so a root declared with a write layer and no ordinary source
    /// was missing from that report entirely — silently absent from the one
    /// place that says whether writes copy up.
    #[test]
    fn composed_roots_includes_a_root_that_has_only_a_write_layer() {
        let upper = dir("only-upper", "upper.txt");
        let content = dir("with-source", "content.txt");

        let s = Session::new();
        s.mount_at(RootId(1), "", Arc::new(DiskProvider::new(&content))).unwrap();
        s.set_write_layer_at(RootId(2), Arc::new(DiskProvider::new(&upper))).unwrap();

        assert_eq!(
            s.composed_roots(),
            vec![RootId(1), RootId(2)],
            "a write-layer-only root must be enumerated too, ascending"
        );
        assert!(!s.has_write_layer(RootId(1)));
        assert!(s.has_write_layer(RootId(2)));
        // Root 0 was never touched, so it is not composed and must not appear.
        assert!(!s.composed_roots().contains(&RootId::DEFAULT));
    }

    /// The check is about ownership, not about recomposition: a root the
    /// session already composes keeps composing, however many times.
    #[test]
    fn a_session_composed_root_recomposes_as_often_as_asked() {
        let first = dir("first", "first.txt");
        let second = dir("second", "second.txt");

        let s = Session::new();
        s.mount_at(RootId(2), "", Arc::new(DiskProvider::new(&first))).unwrap();
        s.mount_at(RootId(2), "", Arc::new(DiskProvider::new(&second))).unwrap();
        s.set_write_layer_at(RootId(2), Arc::new(DiskProvider::new(&second)))
            .unwrap();
        s.set_root_mounts(
            RootId(2),
            vec![(String::new(), Arc::new(DiskProvider::new(&first)))],
        )
        .unwrap();

        assert!(s.kernel().getattr(RootId(2), "first.txt").unwrap().is_some());
        assert!(
            s.has_write_layer(RootId(2)),
            "the write layer must survive a later set_root_mounts"
        );
        assert!(
            s.kernel().open(RootId(2), "first.txt", vfs_provider::OPEN_WRITE).is_ok(),
            "with a write layer, an in-place edit of the read side must copy up"
        );

        // Nothing leaked into root 0, which this session never composed.
        assert!(
            s.kernel()
                .getattr(RootId::DEFAULT, "first.txt")
                .unwrap()
                .is_none(),
            "an uncomposed root must answer for nothing, not for another root's content"
        );
    }
}

// The golden is consumed by a `shim.cfg` write on both targets now, so the
// constant, the two helpers that decode it and this test are all portable.
#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn empty_tree_snapshot_is_valid_header() {
        let snap = empty_tree_snapshot();
        assert_eq!(snap.len(), 128);
        // MAGIC "SSFV" little-endian = 0x5646_5353
        assert_eq!(&snap[0..4], &[0x53, 0x53, 0x46, 0x56]);
        assert_eq!(u32::from_le_bytes(snap[4..8].try_into().unwrap()), 1);
    }
}

#[cfg(test)]
mod launch_image_tests {
    use super::*;
    use vfs_director::DiskProvider;

    /// Minimal PE32+ with no imports — staging parses the import table, so
    /// the bytes must be a real (if empty) PE. Same shape as
    /// `vfs-directord/tests/staging.rs`'s `bare_pe`.
    fn bare_pe() -> Vec<u8> {
        let mut pe = vec![0u8; 0x400];
        pe[0] = b'M';
        pe[1] = b'Z';
        pe[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
        pe[0x94..0x96].copy_from_slice(&240u16.to_le_bytes());
        pe[0x98..0x9A].copy_from_slice(&0x20Bu16.to_le_bytes());
        pe
    }

    fn content(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("vfs-li-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("game.exe"), bare_pe()).unwrap();
        p
    }

    #[test]
    fn a_graph_only_image_in_root_zero_is_staged_into_the_root() {
        let c = content("stage");
        let s = Session::new();
        s.mount("", Arc::new(DiskProvider::new(&c))).unwrap();
        let loc0 = s.root_locations()[0].location.clone();
        let img = image::join_location(&loc0, "game.exe");
        let host = s.resolve_for_test(&LaunchOpts { image: img, ..Default::default() }).unwrap();
        assert!(host.is_file(), "staged image must exist on the host: {}", host.display());
        assert!(host.starts_with(s.virtual_root()), "staged into root 0's backing dir");
    }

    #[test]
    fn a_graph_only_image_in_another_root_is_refused_by_name() {
        let c = content("r1");
        let mut s = Session::new();
        let loc1 = if cfg!(windows) {
            std::env::temp_dir().join(format!("vfs-li-r1loc-{}", std::process::id()))
                .to_string_lossy().into_owned()
        } else {
            r"C:\users\steamuser\Saves".to_string()
        };
        s.declare_root(1, &loc1);
        s.mount_at(RootId(1), "", Arc::new(DiskProvider::new(&c))).unwrap();
        let e = s
            .resolve_for_test(&LaunchOpts { image: image::join_location(&loc1, "game.exe"), ..Default::default() })
            .unwrap_err();
        assert!(e.contains("root 1") && e.contains("root 0"), "{e}");
    }

    #[test]
    fn an_image_no_root_serves_is_refused() {
        let s = Session::new();
        let e = s.resolve_for_test(&LaunchOpts { image: "missing.exe".into(), ..Default::default() }).unwrap_err();
        // "nothing to stage" is the Windows launch's refusal text, which
        // `embed_api.rs`'s Windows-only launch test asserts on.
        assert!(e.contains("missing.exe") && e.contains("nothing to stage"), "{e}");
    }

    /// A drive-relative `C:foo.exe` is relative to `classify_image`, so it
    /// lands in root 0 — but on Windows `Path::join` of a component with a
    /// drive replaces the whole base. Refused on every target, by name, even
    /// where a file of that name really exists in the root (unix allows it).
    #[test]
    fn a_vpath_component_with_a_colon_is_refused_by_name() {
        let s = Session::new();
        std::fs::create_dir_all(s.virtual_root().join("bin")).unwrap();
        if cfg!(unix) {
            std::fs::write(s.virtual_root().join("C:foo.exe"), bare_pe()).unwrap();
            std::fs::write(s.virtual_root().join("bin").join("D:x.exe"), bare_pe()).unwrap();
        }
        for img in ["C:foo.exe", r"bin\D:x.exe"] {
            let e = s
                .resolve_for_test(&LaunchOpts { image: img.into(), ..Default::default() })
                .unwrap_err();
            assert!(e.contains(&format!("{img:?}")) && e.contains("':'"), "{img}: {e}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_root_zero_location_defaults_and_can_be_declared() {
        let mut s = Session::new();
        assert_eq!(s.root_locations()[0].location, r"C:\vfs-session\root");
        s.declare_root(0, r"C:\Games\Fixture");
        assert_eq!(s.root_locations()[0].location, r"C:\Games\Fixture");
        assert_ne!(s.virtual_root(), Path::new(r"C:\Games\Fixture"), "virtual_root stays the host dir");
    }

    #[cfg(unix)]
    #[test]
    fn unix_host_path_outside_every_root_is_refused() {
        let s = Session::new();
        let e = s.resolve_for_test(&LaunchOpts { image: "/usr/bin/true".into(), ..Default::default() }).unwrap_err();
        assert!(e.contains("no drive"), "{e}");
    }

    /// The accepted pre-location form: an absolute host path inside the
    /// managed root is the same as its relative remainder.
    #[cfg(unix)]
    #[test]
    fn unix_host_path_inside_the_managed_root_is_root_zero() {
        let s = Session::new();
        std::fs::create_dir_all(s.virtual_root().join("bin")).unwrap();
        let real = s.virtual_root().join("bin").join("real.exe");
        std::fs::write(&real, bare_pe()).unwrap();
        let host = s
            .resolve_for_test(&LaunchOpts { image: real.to_string_lossy().into_owned(), ..Default::default() })
            .unwrap();
        assert_eq!(host, real);
    }

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
        let p = std::env::temp_dir().join(format!("vfs-lr-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
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
        let home = std::env::temp_dir().join(format!("vfs-drop-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
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
        let mut s = Session::new();
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
