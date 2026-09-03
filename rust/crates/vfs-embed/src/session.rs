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
use vfs_director::stage::{stage_launch_into, ImageSource, StagedDir};
use vfs_director::{Director, DiskProvider, MountGraph};
// The Proton delivery mechanism: the unix counterpart of the `vfs-inject` +
// `vfs-shim` pair, carrying GE-Proton discovery, the per-session Wine prefix,
// and the injector's positional argv plus the shim's env handshake. Gated in
// the manifest too (`[target.'cfg(unix)'.dependencies]`).
#[cfg(unix)]
use vfs_proton::{launch::WineLaunch, layout::Root as ProtonRoot, prefix::Prefix};
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
/// Held on both targets, by different writers. On Windows: `serve`'s
/// `apply_env_roots` and `launch`'s `opts.env` save/set/restore. On unix only
/// the latter — a Wine child is handed an explicit environment block built by
/// `vfs_proton::launch::launch_env`, so this session's ring coordinates never
/// travel through this process's environment there. `opts.env` still does,
/// because `Command::envs` adds to the parent environment and `LaunchOpts` has
/// no per-child block of its own.
static LAUNCH_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Options for [`Session::launch`].
#[derive(Clone, Debug)]
pub struct LaunchOpts {
    /// Path to the image to launch.
    ///
    /// An **absolute** path is launched as given — it is a real file and no
    /// lookup of ours applies to it.
    ///
    /// A **relative** name is resolved in two steps: first joined onto the
    /// managed root and looked for on real disk (a host whose root is a real
    /// game directory), and failing that looked up as a vpath in root 0's
    /// **provider graph**, in which case [`Session::launch`] stages it — see
    /// there. A name neither holds is refused by name rather than handed to
    /// `CreateProcess`.
    pub image: String,
    pub args: Vec<String>,
    /// Wait for process exit (false = detach; session must stay alive).
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
    /// in. None of the three can be built on Linux, so a missing one is
    /// reported by name (see `locate_wine_artifacts`) rather than surfacing as
    /// a path error out of `wine`.
    pub shim_dll: Option<String>,
    pub payload_dll: Option<String>,
    /// Extra environment variables for the child.
    ///
    /// **Not child-only.** `CreateProcessW` is called with a null environment
    /// block — inheritance *is* the mechanism — so [`Session::launch`] writes
    /// each one into **this process's** environment with `std::env::set_var`,
    /// launches, and restores the previous value. [`LAUNCH_ENV_LOCK`] serializes
    /// that against every other env write this crate performs, so two sessions
    /// cannot interleave; it cannot serialize a host's *own* threads, and
    /// `set_var` in a multi-threaded process races anything else reading the
    /// environment. See [`Session::launch`]'s "Process-global environment"
    /// section for the costed fix.
    pub env: BTreeMap<String, String>,
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
/// Windows-only: the sole constructor is `launch`'s staging step, which is
/// itself `#[cfg(windows)]`. `stage_launch` stays portable — it takes any
/// `&dyn ImageSource` a host supplies.
#[cfg(windows)]
struct KernelSource(Arc<Director>);

#[cfg(windows)]
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
    /// Host directories for the session's roots **beyond root 0**, which
    /// `virtual_root` names — see [`Session::declare_root`].
    extra_roots: Vec<(u32, PathBuf)>,
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
            staged: Mutex::new(None),
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
    /// (A *daemon* session is a different matter: there root 0 is a directory
    /// the daemon created and already published to its client, so
    /// `SessionRegistry::declare_root` refuses id 0 above this layer. That is
    /// a policy of that host, not of embedding.)
    ///
    /// Re-declaring an id replaces its path. Takes effect at the next
    /// [`Session::serve`] or [`Session::launch`], which is what publishes it
    /// into the environment the child inherits.
    pub fn declare_root(&mut self, id: u32, path: impl Into<PathBuf>) {
        let path = path.into();
        if id == 0 {
            self.virtual_root = path;
            return;
        }
        match self.extra_roots.iter_mut().find(|(r, _)| *r == id) {
            Some(slot) => slot.1 = path,
            None => self.extra_roots.push((id, path)),
        }
    }

    /// The roots declared beyond root 0, in declaration order. For
    /// diagnostics and for tests that need to prove a config's `[[root]]`
    /// table actually reached the session rather than being parsed and
    /// dropped.
    pub fn declared_roots(&self) -> &[(u32, PathBuf)] {
        &self.extra_roots
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
    /// that protocol carries no `VFS_VIRTUAL_ROOTS` at all — which is why the
    /// unix `launch` refuses a session declaring a root beyond root 0 instead
    /// of launching a child that would classify its paths as nobody's.
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
    /// **Never wrap the upper in a [`crate::CachingProvider`].** A host is
    /// expected to put slow sources behind the block cache (spec §8b) and it
    /// is natural to do that uniformly, in one loop, over everything it
    /// mounts. The write layer is the one provider in the graph whose bytes
    /// change underneath the director: a cached read of a file that was just
    /// copied up serves the pre-write content, and the symptom is a game
    /// reading back its own edit as the original. `vfs-directord` caches every
    /// source and exempts the layer here for that reason.
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
        let ipc = IpcServe::start(Arc::clone(&self.kernel), section)?;
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
        let ipc = IpcServe::start_file_backed(
            Arc::clone(&self.kernel),
            &ring,
            PROTON_PAYLOAD_CAP,
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
    /// The hash is `DefaultHasher`, whose output is stable within a build but
    /// not promised across Rust releases. That is fine for what it names: a
    /// rebuilt host boots one new prefix and reuses it from then on.
    #[cfg(unix)]
    fn wine_session_id(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.state_dir.hash(&mut h);
        format!("session-{:016x}", h.finish())
    }

    /// Links the managed root, the overlay and the state directory into
    /// `prefix/drive_c/vfs-session/`, and returns the three `C:\` paths they
    /// are reachable at, in that order.
    ///
    /// **A Wine process can only name what is under one of its drives**, and
    /// these three live wherever the host put them — normally under `/tmp`,
    /// outside the prefix entirely. Symlinks into `drive_c` rather than a
    /// `dosdevices` letter each ([`Prefix::map_drive`]): one location instead
    /// of three letters to allocate, [`Prefix::windows_path`] renders the
    /// result, and every path the shim is handed is a subdirectory rather than
    /// a bare drive root.
    ///
    /// Each link is replaced, not created-if-absent: a session relaunches into
    /// the prefix it already booted, and `set_root` may have moved the target
    /// in between.
    #[cfg(unix)]
    fn link_into_prefix(&self, prefix: &Prefix) -> Result<(String, String, String), String> {
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
        Ok((
            link("root", &self.virtual_root)?,
            link("overlay", &self.overlay)?,
            link("state", &self.state_dir)?,
        ))
    }

    /// Launch `opts.image` under the virtual root with dual-layer inject.
    /// Child sees remapped I/O for paths under `virtual_root`.
    ///
    /// Requires [`serve`] first. On `wait: false`, keep this `Session` alive.
    ///
    /// ## How `image` is resolved
    ///
    /// Absolute: launched as given.
    ///
    /// Relative: joined onto the virtual root and launched from there if a
    /// real file exists (a host whose root is a real game directory —
    /// fixtures, a vanilla install). Otherwise looked up as a **vpath in root
    /// 0's provider graph**, and if the graph holds it, staged — see below.
    /// Neither: refused by name, because `CreateProcess` would only fail
    /// later and less clearly.
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

        let root_s = self.virtual_root.to_string_lossy().into_owned();
        let image_path = Path::new(&opts.image);
        let target = if image_path.is_absolute() {
            image_path.to_path_buf()
        } else {
            let on_disk = self.virtual_root.join(&opts.image);
            if on_disk.is_file() {
                on_disk
            } else if self
                .kernel
                .getattr(RootId::DEFAULT, &opts.image)
                .ok()
                .flatten()
                .is_some()
            {
                // VFS content. Write it (and its import closure) out, mount
                // the staging directory back under the curated graph, and
                // launch the real file that produces.
                let also: Vec<&str> = opts.stage_also.iter().map(String::as_str).collect();
                self.stage_launch(
                    &KernelSource(Arc::clone(&self.kernel)),
                    &StageOpts {
                        exe_vpath: &opts.image,
                        also: &also,
                        fallback_dirs: &opts.stage_fallback_dirs,
                    },
                )
                .map_err(|e| format!("launch: staging {:?}: {e}", opts.image))?
            } else {
                return Err(format!(
                    "launch: {:?} resolves to {}, which does not exist — and this session's \
                     provider graph does not serve it either, so there is nothing to stage. A \
                     relative LaunchOpts.image must be a real file under the managed root or a \
                     vpath root 0 serves.",
                    opts.image,
                    on_disk.display()
                ));
            }
        };
        let config_path = self.state_dir.join("shim.cfg");
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

        let thin = self.state_dir.join("fuse.cfg");
        // Re-published here, not only in `serve`: the launch env lock is held
        // from this point, and a root declared after `serve` (or by another
        // session sharing this process's environment) must reach this child.
        ipc.apply_env_roots(&root_s, &self.extra_roots_env(), &thin);

        let mut saved: Vec<(String, Option<String>)> = Vec::with_capacity(opts.env.len());
        for (k, v) in &opts.env {
            saved.push((k.clone(), std::env::var(k).ok()));
            std::env::set_var(k, v);
        }

        let ready_timeout = vfs_env::text(vfs_env::READY_TIMEOUT_SECS).ok_or(())
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(180));

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
    /// Four things a Wine launch needs that a Windows one does not:
    ///
    /// 1. **A runtime.** The newest verified GE-Proton under this host's
    ///    aether-vfs home (`VFS_HOME`, else `XDG_DATA_HOME`, else `$HOME` —
    ///    `vfs_proton::layout::Root::from_env`). Never a fallback to stock
    ///    Proton: `vfs_proton::launch::run` re-verifies before it spawns.
    /// 2. **A prefix**, this session's own, keyed by `state_dir`
    ///    ([`Session::wine_session_id`]).
    /// 3. **`C:\` names for the session's directories**, which live outside
    ///    the prefix ([`Session::link_into_prefix`]) — and `shim.cfg` written
    ///    *here* rather than in `serve`, since it carries two of them.
    /// 4. **This ring's real geometry**, taken straight off the live
    ///    [`IpcServe`]: `map_bytes`, `arena_offset`, `arena_len`,
    ///    `payload_cap`. The shim defaults `VFS_RING_BYTES` to 2 MiB, and that
    ///    default over a ~34 MiB ring attaches cleanly, answers a 256 KiB read
    ///    and fails only at 4 MiB — measured, see `vfs_proton::launch`.
    ///    Nothing here may pass a default in their place.
    ///
    /// Three of [`LaunchOpts`]' knobs are **refused rather than ignored**,
    /// because dropping any of them silently produces a child that runs and is
    /// wrong: `wait: false` (nothing here can detach — `run` waits), a session
    /// with roots beyond root 0 (the file-backed env protocol carries no root
    /// map, so those paths would fall through to real disk inside Wine), and an
    /// `image` only the provider graph holds (staging is not wired to this
    /// path). `stage_also` / `stage_fallback_dirs` are staging knobs and so are
    /// unused here, exactly as their docs say.
    #[cfg(unix)]
    pub fn launch(&self, opts: &LaunchOpts) -> Result<i32, String> {
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
        if !opts.wait {
            return Err("launch: wait: false (detach) is not supported on the Proton path — \
                        vfs_proton::launch::run waits for the child and returns its exit \
                        code. Returning after the wait while reporting a detach would be a \
                        launch that lied about when it finished."
                .to_string());
        }
        if !self.extra_roots.is_empty() {
            return Err(format!(
                "launch: this session declares {} root(s) beyond root 0, and the file-backed \
                 env protocol carries no root map (see vfs_proton::launch::launch_env, which \
                 sets VFS_VIRTUAL_DIR and deliberately not VFS_VIRTUAL_ROOTS). A root the \
                 shim is never told about is one whose paths it classifies as nobody's and \
                 lets fall through to real disk, silently — so this is refused rather than \
                 launched with roots missing.",
                self.extra_roots.len()
            ));
        }

        let home = ProtonRoot::from_env()
            .map_err(|e| format!("launch: no aether-vfs home (set VFS_HOME): {e}"))?;
        // Which Wine, and how its prefixes are made, is the only part of this
        // body that differs between Linux and macOS. See `wine_host`.
        let host = crate::wine_host::resolve(&home)?;
        let runtime = crate::wine_host::runtime_path(&host);
        let prefix = crate::wine_host::ensure_prefix(&host, &home, &self.wine_session_id())?;
        let (wine_root, wine_overlay, wine_state) = self.link_into_prefix(&prefix)?;

        // The ring as the shim sees it. Its bytes are the same inode `serve`
        // created; only the name differs.
        let ring_name = ring
            .file_name()
            .ok_or_else(|| format!("launch: ring path {} has no file name", ring.display()))?;
        let wine_ring = join_wine(&wine_state, Path::new(ring_name))?;

        let target = self.wine_target(opts, &wine_root)?;

        // Written here, not in `serve`: these are the root and overlay *as the
        // shim sees them*, and neither existed as a `C:\` name until the prefix
        // above did. The snapshot must still be a valid empty tree —
        // `Engine::build` rejects zero-length snapshot bytes, which would abort
        // dual-layer bootstrap before hooks install.
        let config_path = self.state_dir.join("shim.cfg");
        let snap = empty_tree_snapshot();
        std::fs::write(
            &config_path,
            vfs_protocol::shimcfg::encode_config_with_overlay(&wine_root, &wine_overlay, &snap),
        )
        .map_err(|e| format!("launch: write {}: {e}", config_path.display()))?;

        let ready_path = self.state_dir.join("ready.flag");
        let _ = std::fs::remove_file(&ready_path);

        let (injector, shim_dll, payload_dll) = locate_wine_artifacts(opts)?;

        let wine = WineLaunch {
            runtime,
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
            virtual_dir: wine_root,
            args: opts.args.clone(),
        };

        // `opts.env` reaches the child by inheritance here too: `run` builds the
        // child's `VFS_*`/Wine block explicitly but adds it *to* this process's
        // environment (`Command::envs`). Same lock and same restore as the
        // Windows body, for the same reason — see [`LAUNCH_ENV_LOCK`].
        let _guard = LAUNCH_ENV_LOCK
            .lock()
            .map_err(|_| "launch env lock poisoned".to_string())?;
        let mut saved: Vec<(String, Option<String>)> = Vec::with_capacity(opts.env.len());
        for (k, v) in &opts.env {
            saved.push((k.clone(), std::env::var(k).ok()));
            std::env::set_var(k, v);
        }

        let exit = crate::wine_host::run(&host, &wine);

        for (k, old) in saved {
            match old {
                Some(v) => std::env::set_var(&k, v),
                None => std::env::remove_var(&k),
            }
        }

        // The Wine is named in the failure, not only in a success log: a launch
        // that dies inside the runtime is exactly when "which Wine was this?"
        // is the first question, and the answer is otherwise nowhere in the
        // message.
        exit.map_err(|e| format!("launch: {e} (host: {})", crate::wine_host::describe(&host)))
    }

    /// `opts.image` as the **child** must name it, or a refusal saying which
    /// of the three forms it failed.
    ///
    /// Three accepted forms, all ending in a path the Wine process can open:
    ///
    /// - Already a Windows path (`C:\…`, `\\?\…`, a UNC name): handed through.
    ///   A host that knows the child's own path space is not second-guessed.
    /// - Relative: resolved under the managed root, which the child reaches as
    ///   `<wine_root>\…`.
    /// - An absolute **host** path inside the managed root: the same case,
    ///   rewritten. Outside it there is no drive to name it by, so it is
    ///   refused rather than passed to `wine` as a Unix path.
    ///
    /// **A vpath only the provider graph serves is refused**, unlike the
    /// Windows body, which stages it. `CreateProcess` inside Wine reads the
    /// image before any hook of ours exists, exactly as on Windows, so staging
    /// would be needed — but the staged directory has to be nameable from
    /// inside Wine and mounted back under the curated graph, and none of that
    /// is wired to this path yet. Refused by name beats a `wine` error about a
    /// file nobody can see.
    #[cfg(unix)]
    fn wine_target(&self, opts: &LaunchOpts, wine_root: &str) -> Result<String, String> {
        if is_windows_path(&opts.image) {
            return Ok(opts.image.clone());
        }
        let image = Path::new(&opts.image);
        let rel = if image.is_absolute() {
            image.strip_prefix(&self.virtual_root).map_err(|_| {
                format!(
                    "launch: {} is an absolute host path outside the managed root ({}), so no \
                     drive in this session's Wine prefix names it. Put the image under the \
                     managed root, or give it as the child sees it (C:\\...).",
                    image.display(),
                    self.virtual_root.display()
                )
            })?
        } else {
            image
        };
        let on_disk = self.virtual_root.join(rel);
        if !on_disk.is_file() {
            let in_graph = self
                .kernel
                .getattr(RootId::DEFAULT, &opts.image)
                .ok()
                .flatten()
                .is_some();
            return Err(format!(
                "launch: {:?} resolves to {}, which is not a real file{}. The Proton path \
                 launches a real image only — CreateProcess inside Wine reads it before any \
                 hook of ours exists, and staging a graph-only image is not wired to this \
                 path yet.",
                opts.image,
                on_disk.display(),
                if in_graph {
                    " — root 0's provider graph does serve that vpath, so staging is what is \
                     missing, not the content"
                } else {
                    ", and root 0's provider graph does not serve it either"
                }
            ));
        }
        join_wine(wine_root, rel)
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

/// Whether `s` is already a path in the child's own space rather than this
/// host's: `C:\…` / `C:/…`, `\\?\…`, or a UNC name.
///
/// Prefix-only, no filesystem check: this decides which *namespace* the string
/// belongs to, and a Windows path cannot be resolved on the host to confirm it.
#[cfg(unix)]
fn is_windows_path(s: &str) -> bool {
    let b = s.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
        || s.starts_with("\\\\")
}

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
/// **None of them can be built on Linux** — `vfs-injector.exe`,
/// `vfs_shim_dll.dll` and `vfs_payload.dll` are Windows targets — so this
/// resolves what a host has copied in rather than producing anything, and says
/// so in the failure. [`LaunchOpts::shim_dll`] / [`LaunchOpts::payload_dll`]
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
            "launch: these Windows artifacts are missing: {}. They are PE binaries, so a unix \
             host either cross-builds them (`cargo xwin build --target \
             x86_64-pc-windows-msvc -p vfs-inject -p vfs-shim-dll`, plus the separate \
             `crates/vfs-payload` workspace) or copies in the same three built on Windows. \
             Put all three in {}, or set LaunchOpts.shim_dll and LaunchOpts.payload_dll to \
             where they are (vfs-injector.exe is then looked for beside shim_dll).",
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
