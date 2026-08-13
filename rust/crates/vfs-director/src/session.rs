//! Host session: configure mounts + paths, serve IPC, **launch a process** with
//! all NT I/O under the virtual root remapped through this director.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::director::Director;
use crate::ipc::IpcServe;
use crate::ops::{Provider, OPEN_READ};

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

    pub fn mount(&self, prefix: &str, backend: Arc<dyn Provider>) -> Result<(), i32> {
        self.kernel.mount(prefix, backend)
    }

    /// Drop all mounts before rebuilding composition.
    pub fn clear_mounts(&self) -> Result<(), i32> {
        self.kernel.clear_mounts()
    }

    /// Mount a Stored zip archive as a content backend (later mounts win on conflicts).
    ///
    /// Requires the `zip` feature (on by default).
    #[cfg(feature = "zip")]
    pub fn mount_zip(&self, zip_path: impl AsRef<Path>) -> Result<(), String> {
        let path = zip_path.as_ref();
        let be = vfs_zip::ZipBackend::open(path)
            .map_err(|e| format!("ZipBackend {}: {e:?}", path.display()))?;
        self.kernel
            .mount("", Arc::new(be))
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
        let (fh, size, is_dir) = self.kernel.open(vpath, OPEN_READ)?;
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
        ipc.apply_env(&root_s, &thin);

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
        ipc.apply_env(&root_s, &thin);

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
