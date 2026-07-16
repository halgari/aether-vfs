//! Host session: configure mounts + paths, serve IPC, **launch a process** with
//! all NT I/O under the virtual root remapped through this director.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::director::Director;
use crate::ipc::IpcServe;
use crate::ops::{Backend, OPEN_READ};

/// Options for [`Session::launch`].
#[derive(Clone, Debug)]
pub struct LaunchOpts {
    /// Virtual image path under the managed root (e.g. `SkyrimSE.exe` or `skse64_loader.exe`).
    pub image: String,
    pub args: Vec<String>,
    /// Wait for process exit (false = detach; session must stay alive).
    pub wait: bool,
    /// Load PE bytes from the VFS and process-hollow a host image (no PE on disk).
    pub hollow_pe: bool,
    /// Optional override paths for shim/payload DLLs (else search near this exe).
    pub shim_dll: Option<String>,
    pub payload_dll: Option<String>,
}

impl Default for LaunchOpts {
    fn default() -> Self {
        LaunchOpts {
            image: "SkyrimSE.exe".into(),
            args: Vec::new(),
            wait: true,
            hollow_pe: true,
            shim_dll: None,
            payload_dll: None,
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

    pub fn mount(&self, prefix: &str, backend: Arc<dyn Backend>) -> Result<(), i32> {
        self.kernel.mount(prefix, backend)
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

        // Minimal shim.cfg (FUSE path is env-driven; snapshot optional).
        let overlay_s = self.overlay.to_string_lossy().into_owned();
        let snap: Vec<u8> = Vec::new();
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
    pub fn launch(&self, opts: &LaunchOpts) -> Result<i32, String> {
        let ipc = self
            .ipc
            .as_ref()
            .ok_or_else(|| "serve() before launch()".to_string())?;

        let root_s = self.virtual_root.to_string_lossy().into_owned();
        let target = self.virtual_root.join(&opts.image);
        let config_path = self.state_dir.join("shim.cfg");
        let ready_path = self.state_dir.join("ready.flag");
        let _ = std::fs::remove_file(&ready_path);

        let (dll, payload) = locate_shim_payload(opts)?;
        let pe_bytes = if opts.hollow_pe {
            Some(
                self.read_file(&opts.image.replace('\\', "/"))
                    .map_err(|e| format!("read PE from VFS {}: status {e}", opts.image))?,
            )
        } else {
            None
        };

        // Ensure env still points at this IPC (in case host changed process env).
        let thin = self.state_dir.join("fuse.cfg");
        ipc.apply_env(&root_s, &thin);

        let exit = vfs_inject::run_target_with_shim(vfs_inject::RunConfig {
            target_exe: target.to_string_lossy().into_owned(),
            args: opts.args.clone(),
            current_dir: Some(root_s),
            dll_path: dll,
            config_path: config_path.to_string_lossy().into_owned(),
            ready_path: ready_path.to_string_lossy().into_owned(),
            ready_timeout: Duration::from_secs(120),
            payload_path: payload,
            preinit_redirects: vec![],
            detach: !opts.wait,
            target_pe_bytes: pe_bytes,
        })
        .map_err(|e| format!("launch: {e:?}"))?;

        Ok(exit)
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
