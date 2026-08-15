//! Host session: configure mounts + paths, serve IPC, **launch a process** with
//! all NT I/O under the virtual root remapped through this director.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::director::Director;
use crate::ipc::IpcServe;
use crate::mount_graph::MountGraph;
use crate::ops::{Access, Provider, RootId, OPEN_READ};

/// Serializes process-global env mutation around [`Session::launch`].
///
/// `CreateProcessW` inherits the parent's environment (null env block), and
/// `IpcServe::apply_env` / `run_target_with_shim` both set process-wide `VFS_*`
/// vars. A multi-session daemon must not interleave two launches.
static LAUNCH_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Options for [`Session::launch`].
#[derive(Clone, Debug)]
pub struct LaunchOpts {
    /// Absolute path to the image to launch — normally the staged EXE. A
    /// relative name resolves under the managed root (fixtures / tools).
    pub image: String,
    pub args: Vec<String>,
    /// Wait for process exit (false = detach; session must stay alive).
    pub wait: bool,
    /// Optional override paths for shim/payload DLLs (else search near this exe).
    pub shim_dll: Option<String>,
    pub payload_dll: Option<String>,
    /// Extra environment variables for the child only. Applied under a process
    /// lock around launch and restored afterward so they do not leak into the
    /// host / other sessions.
    pub env: BTreeMap<String, String>,
}

impl Default for LaunchOpts {
    fn default() -> Self {
        LaunchOpts {
            image: "SkyrimSE.exe".into(),
            args: Vec::new(),
            wait: true,
            shim_dll: None,
            payload_dll: None,
            env: BTreeMap::new(),
        }
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
/// `ST_BAD_REQUEST` if the upper is not `Access::ReadWrite`, or if a mount
/// prefix does not normalize.
pub fn compose_root(
    mounts: Vec<(String, Arc<dyn Provider>)>,
    write_layer: Option<Arc<dyn Provider>>,
) -> Result<Arc<dyn Provider>, i32> {
    let graph: Arc<dyn Provider> = Arc::new(MountGraph::new(mounts)?);
    match write_layer {
        Some(upper) => Ok(Arc::new(
            vfs_compose::OverlayProvider::from_arcs(graph, upper)
                .map_err(|_| crate::ops::bad_request())?,
        )),
        None => Ok(graph),
    }
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
}

impl Session {
    pub fn new() -> Self {
        let tmp = std::env::temp_dir().join(format!("vfs-session-{}", std::process::id()));
        Session {
            kernel: Arc::new(Director::new()),
            virtual_root: tmp.join("root"),
            overlay: tmp.join("overlay"),
            state_dir: tmp.join("state"),
            ipc: None,
            roots: Mutex::new(BTreeMap::new()),
            extra_roots: Vec::new(),
        }
    }

    pub fn kernel(&self) -> &Arc<Director> {
        &self.kernel
    }

    pub fn set_root(&mut self, path: impl Into<PathBuf>) {
        self.virtual_root = path.into();
    }

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
        vfs_shim::overlay_layer_dir(&self.overlay, root)
    }

    /// Declare a second (third, …) managed root: the host directory that
    /// `RootId(id)` virtualizes. Root `0` is [`Session::set_root`] and cannot
    /// be declared here.
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
    /// Re-declaring an id replaces its path. Takes effect at the next
    /// [`Session::serve`] or [`Session::launch`], which is what publishes it
    /// into the environment the child inherits.
    pub fn declare_root(&mut self, id: u32, path: impl Into<PathBuf>) {
        let path = path.into();
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
    fn extra_roots_env(&self) -> Vec<(u32, String)> {
        self.extra_roots
            .iter()
            .filter(|(id, _)| *id != 0)
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
    pub fn mount_at(
        &self,
        root: RootId,
        prefix: &str,
        backend: Arc<dyn Provider>,
    ) -> Result<(), i32> {
        {
            let mut roots = self.roots.lock().map_err(|_| crate::ops::map_io_err())?;
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
            return Err(crate::ops::exists());
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
    pub fn set_root_mounts(
        &self,
        root: RootId,
        mounts: Vec<(String, Arc<dyn Provider>)>,
    ) -> Result<(), i32> {
        {
            let mut roots = self.roots.lock().map_err(|_| crate::ops::map_io_err())?;
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
    pub fn set_write_layer_at(&self, root: RootId, upper: Arc<dyn Provider>) -> Result<(), i32> {
        if upper.capabilities().access != Access::ReadWrite {
            return Err(crate::ops::bad_request());
        }
        {
            let mut roots = self.roots.lock().map_err(|_| crate::ops::map_io_err())?;
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
            .map_err(|_| crate::ops::map_io_err())?
            .get(&root.0)
            .cloned()
            .unwrap_or_default();
        let composed = compose_root(composition.mounts, composition.write_layer)?;
        self.kernel.mount(root, composed)
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
    pub fn clear_root(&self, root: RootId) -> Result<(), i32> {
        self.roots
            .lock()
            .map_err(|_| crate::ops::map_io_err())?
            .remove(&root.0);
        self.kernel.unmount(root)
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
    pub fn ipc(&self) -> Option<&IpcServe> {
        self.ipc.as_ref()
    }

    /// Occasional host-side full-file read (not the primary API).
    pub fn read_file(&self, vpath: &str) -> Result<Vec<u8>, i32> {
        let (fh, size, is_dir) = self.kernel.open(RootId::DEFAULT, vpath, OPEN_READ)?;
        if is_dir {
            let _ = self.kernel.close(fh);
            return Err(crate::ops::is_dir());
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

    /// Start the control ring + workers so an injected child can remap I/O.
    /// Idempotent if already serving.
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
        ipc.apply_env_roots(&root_s, &self.extra_roots_env(), &thin);

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

    /// Launch `opts.image` under the virtual root with dual-layer inject.
    /// Child sees remapped I/O for paths under `virtual_root`.
    ///
    /// Requires [`serve`] first. On `wait: false`, keep this `Session` alive.
    ///
    /// An absolute `image` is launched directly; a relative one resolves under
    /// the virtual root.
    pub fn launch(&self, opts: &LaunchOpts) -> Result<i32, String> {
        let ipc = self
            .ipc
            .as_ref()
            .ok_or_else(|| "serve() before launch()".to_string())?;

        let root_s = self.virtual_root.to_string_lossy().into_owned();
        let image_path = Path::new(&opts.image);
        let target = if image_path.is_absolute() {
            image_path.to_path_buf()
        } else {
            self.virtual_root.join(&opts.image)
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
    use crate::disk::DiskProvider;
    use crate::ops::ST_EXISTS;

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
            s.kernel().open(RootId(2), "first.txt", crate::ops::OPEN_WRITE).is_ok(),
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
