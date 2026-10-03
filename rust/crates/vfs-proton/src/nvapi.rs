//! NVIDIA NVAPI and NGX (DLSS) under Proton, done the way the `proton` script
//! does it.
//!
//! A launch here runs the runtime's `files/bin/wine` directly, so nothing the
//! `proton` script does per launch happens unless this crate does it. GE-Proton
//! 11-7's script, with NVAPI on (its default; only the `disablenvapi` compat
//! flag or `PROTON_DISABLE_NVAPI=1` turns it off):
//!
//! - `setup_prefix` copies DXVK-NVAPI from the runtime into the prefix:
//!   `lib/wine/nvapi/x86_64-windows/{nvapi64,nvofapi64}.dll` into `system32`
//!   and `lib/wine/nvapi/i386-windows/nvapi.dll` into `syswow64`, and sets the
//!   overrides `nvapi64=n`, `nvofapi64=n`, `nvapi=n` and `nvcuda=b`.
//! - It copies the NVIDIA driver's Wine NGX DLLs, `_nvngx.dll` and
//!   `nvngx.dll`, from the driver's `nvidia/wine` directory into `system32`,
//!   and exports that directory as `NVIDIA_WINE_DLL_DIR`, where NGX looks for
//!   the rest (`nvngx_dlssg.dll`) when it cannot find a DriverStore. The
//!   script finds the directory by `dlopen`ing `libGLX_nvidia.so.0` and going
//!   to `<its real dir>/nvidia/wine`; [`find_ngx_dir`] looks in the same place
//!   without loading the driver into this process, then in the usual fixed
//!   locations.
//! - The session sets `DXVK_ENABLE_NVAPI=1` (otherwise DXVK reports an NVIDIA
//!   GPU as AMD and NVAPI finds no adapter) and, unless already set,
//!   `DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS=DLSSIndicator=0,DLSSGIndicator=0,`.
//! - With the NVIDIA kernel module loaded it turns on wine-nvml, which puts
//!   `lib/wine/nvidia-libs/nvml/wine` first in `WINEDLLPATH`.
//!
//! The script does the copies whatever the GPU is; this crate does them only
//! when an NVIDIA driver is loaded, so a launch on any other machine is
//! unchanged. Turning it off on an NVIDIA machine ([`remove`]) deletes the two
//! NVAPI DLLs as the script does when NVAPI is disabled.

use std::io;
use std::path::{Path, PathBuf};

/// `WINEDLLOVERRIDES` entries the `proton` script sets when NVAPI is on.
pub const NVAPI_OVERRIDES: &str = "nvapi64=n;nvofapi64=n;nvapi=n;nvcuda=b";
/// `DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS` unless the host sets its own: no DLSS
/// on-screen indicator.
pub const NGX_DEBUG_OPTIONS: &str = "DLSSIndicator=0,DLSSGIndicator=0,";
/// The NGX DLLs the script copies from the driver's Wine directory.
pub const NGX_DLLS: [&str; 2] = ["_nvngx.dll", "nvngx.dll"];

/// Where host facts are read from: `/` for this machine, a fake tree in tests.
/// Every absolute path below (`/proc/...`, `/sys/...`, `/usr/lib/...`) is
/// looked up under it.
#[derive(Debug, Clone)]
pub struct Host {
    pub root: PathBuf,
}

impl Host {
    /// This machine.
    pub fn real() -> Host {
        Host {
            root: PathBuf::from("/"),
        }
    }

    fn path(&self, abs: &str) -> PathBuf {
        self.root.join(abs.trim_start_matches('/'))
    }
}

/// What the host has in the way of NVIDIA graphics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gpu {
    /// The NVIDIA kernel driver is loaded. Carries the first line of
    /// `/proc/driver/nvidia/version` when there is one.
    Nvidia(Option<String>),
    /// An NVIDIA display device is on the PCI bus, but the NVIDIA driver is
    /// not loaded (nouveau or nova, or no driver). NVAPI needs NVIDIA's.
    NvidiaWithoutDriver,
    /// No NVIDIA display device.
    None,
}

/// Detects the NVIDIA driver: `/proc/driver/nvidia/version`, or an `nvidia`
/// line in `/proc/modules` (the script's own check); failing both, whether
/// any PCI display device (class `0x03....`) has vendor `0x10de`.
pub fn detect_gpu(host: &Host) -> Gpu {
    if let Ok(v) = std::fs::read_to_string(host.path("/proc/driver/nvidia/version")) {
        let first = v
            .lines()
            .next()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty());
        return Gpu::Nvidia(first);
    }
    if let Ok(m) = std::fs::read_to_string(host.path("/proc/modules")) {
        if m.lines().any(|l| l.split(' ').next() == Some("nvidia")) {
            return Gpu::Nvidia(None);
        }
    }
    let nvidia_display = std::fs::read_dir(host.path("/sys/bus/pci/devices"))
        .map(|rd| {
            rd.flatten().any(|d| {
                let read = |f: &str| {
                    std::fs::read_to_string(d.path().join(f))
                        .map(|s| s.trim().to_ascii_lowercase())
                        .unwrap_or_default()
                };
                read("vendor") == "0x10de" && read("class").starts_with("0x03")
            })
        })
        .unwrap_or(false);
    if nvidia_display {
        Gpu::NvidiaWithoutDriver
    } else {
        Gpu::None
    }
}

/// Library directories searched for `libGLX_nvidia.so.0`, whose real
/// directory's `nvidia/wine` is where the script finds the NGX DLLs.
const GLX_LIB_DIRS: [&str; 6] = [
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib64",
    "/usr/lib",
    "/lib/x86_64-linux-gnu",
    "/lib64",
    "/usr/local/lib",
];
/// Where distributions put the driver's Wine DLLs, when the library search
/// finds nothing.
const NGX_DIRS: [&str; 3] = [
    "/usr/lib/nvidia/wine",
    "/usr/lib64/nvidia/wine",
    "/usr/lib/x86_64-linux-gnu/nvidia/wine",
];

/// The NVIDIA driver's Wine DLL directory: the first candidate holding
/// `nvngx.dll` (the script's own test). `libGLX_nvidia.so.0`'s real directory
/// plus `nvidia/wine` first, as the script resolves it, then [`NGX_DIRS`].
/// The result is a path on this host (under [`Host::root`]).
pub fn find_ngx_dir(host: &Host) -> Option<PathBuf> {
    let from_glx = GLX_LIB_DIRS.iter().filter_map(|d| {
        let lib = host.path(d).join("libGLX_nvidia.so.0");
        let real = std::fs::canonicalize(&lib).ok()?;
        Some(real.parent()?.join("nvidia").join("wine"))
    });
    from_glx
        .chain(NGX_DIRS.iter().map(|d| host.path(d)))
        .find(|d| d.join("nvngx.dll").is_file())
}

/// One file to put into the prefix: from a host path to a path relative to
/// the prefix directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// What enabling NVAPI does to one launch: [`Setup::install`] before it, and
/// [`Setup::env`] plus [`NVAPI_OVERRIDES`] in its environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    /// The runtime's DXVK-NVAPI DLLs, then the driver's NGX DLLs.
    pub copies: Vec<Copy>,
    /// The driver's Wine DLL directory, exported as `NVIDIA_WINE_DLL_DIR`.
    pub ngx_dir: Option<PathBuf>,
    /// The runtime's wine-nvml directory, put first in `WINEDLLPATH`.
    pub nvml_dir: Option<PathBuf>,
}

fn nvapi_dir(runtime: &Path, arch: &str) -> PathBuf {
    runtime.join("files/lib/wine/nvapi").join(arch)
}

/// The runtime's 64-bit NVAPI DLL; without it there is nothing to enable.
fn runtime_nvapi64(runtime: &Path) -> PathBuf {
    nvapi_dir(runtime, "x86_64-windows").join("nvapi64.dll")
}

/// The launch setup for `runtime` on `host`, or `None` when NVAPI cannot be
/// enabled — no NVIDIA driver loaded, or no `nvapi64.dll` in the runtime.
/// Missing NGX DLLs leave NVAPI on without DLSS, as the script does.
pub fn setup(host: &Host, runtime: &Path) -> Option<Setup> {
    // Absolute: `nvml_dir` goes into `WINEDLLPATH` as it is.
    let runtime = &std::path::absolute(runtime).unwrap_or_else(|_| runtime.to_path_buf());
    if !matches!(detect_gpu(host), Gpu::Nvidia(_)) || !runtime_nvapi64(runtime).is_file() {
        return None;
    }
    let sys32 = Path::new("drive_c/windows/system32");
    let wow64 = Path::new("drive_c/windows/syswow64");
    let mut copies = Vec::new();
    for (arch, dll, dst) in [
        ("x86_64-windows", "nvapi64.dll", sys32),
        ("x86_64-windows", "nvofapi64.dll", sys32),
        ("i386-windows", "nvapi.dll", wow64),
    ] {
        let from = nvapi_dir(runtime, arch).join(dll);
        if from.is_file() {
            copies.push(Copy {
                from,
                to: dst.join(dll),
            });
        }
    }
    let ngx_dir = find_ngx_dir(host);
    if let Some(dir) = &ngx_dir {
        for dll in NGX_DLLS {
            let from = dir.join(dll);
            if from.is_file() {
                copies.push(Copy {
                    from,
                    to: sys32.join(dll),
                });
            }
        }
    }
    let nvml = runtime.join("files/lib/wine/nvidia-libs/nvml/wine");
    Some(Setup {
        copies,
        ngx_dir,
        nvml_dir: nvml.is_dir().then_some(nvml),
    })
}

impl Setup {
    /// Copies every file into `prefix_dir` whose copy there is missing or
    /// differs, through a temporary file renamed into place, and leaves it
    /// writable as the script does. A destination whose directory does not
    /// exist (a prefix with no `syswow64`) is skipped. Returns how many files
    /// were written: `0` on a prefix already set up.
    pub fn install(&self, prefix_dir: &Path) -> io::Result<usize> {
        let mut written = 0;
        for c in &self.copies {
            let dst = prefix_dir.join(&c.to);
            let Some(dir) = dst.parent().filter(|d| d.is_dir()) else {
                continue;
            };
            if same_contents(&c.from, &dst)? {
                continue;
            }
            let name = dst.file_name().unwrap_or_default().to_string_lossy();
            let tmp = dir.join(format!(".{name}.aether-nvapi.tmp"));
            let res = (|| {
                std::fs::copy(&c.from, &tmp)?;
                let mut perm = std::fs::metadata(&tmp)?.permissions();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    perm.set_mode(perm.mode() | 0o220);
                }
                #[cfg(not(unix))]
                perm.set_readonly(false);
                std::fs::set_permissions(&tmp, perm)?;
                std::fs::rename(&tmp, &dst)
            })();
            if let Err(e) = res {
                let _ = std::fs::remove_file(&tmp);
                return Err(io::Error::new(
                    e.kind(),
                    format!("{} -> {}: {e}", c.from.display(), dst.display()),
                ));
            }
            written += 1;
        }
        Ok(written)
    }

    /// The variables the script sets: `DXVK_ENABLE_NVAPI=1`, the driver's
    /// Wine DLL directory as `NVIDIA_WINE_DLL_DIR`, and
    /// `DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS` when `inherited_ngx_debug` is `false`
    /// (the script only defaults it). `WINEDLLPATH` is [`Setup::nvml_dir`]'s.
    pub fn env(&self, inherited_ngx_debug: bool) -> Vec<(String, String)> {
        let mut env = vec![("DXVK_ENABLE_NVAPI".to_string(), "1".to_string())];
        if let Some(d) = &self.ngx_dir {
            env.push((
                "NVIDIA_WINE_DLL_DIR".to_string(),
                d.to_string_lossy().into_owned(),
            ));
        }
        if !inherited_ngx_debug {
            env.push((
                "DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS".to_string(),
                NGX_DEBUG_OPTIONS.to_string(),
            ));
        }
        env
    }
}

/// Whether `dst` exists with exactly `src`'s bytes.
fn same_contents(src: &Path, dst: &Path) -> io::Result<bool> {
    let d = match std::fs::symlink_metadata(dst) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if !d.is_file() || d.len() != std::fs::metadata(src)?.len() {
        return Ok(false);
    }
    Ok(std::fs::read(src)? == std::fs::read(dst)?)
}

/// What the script does with NVAPI disabled: deletes `system32/nvapi64.dll`
/// and `syswow64/nvapi.dll` (and their `.debug` links) from the prefix.
pub fn remove(prefix_dir: &Path) -> io::Result<()> {
    for f in [
        "drive_c/windows/system32/nvapi64.dll",
        "drive_c/windows/system32/nvapi64.dll.debug",
        "drive_c/windows/syswow64/nvapi.dll",
        "drive_c/windows/syswow64/nvapi.dll.debug",
    ] {
        match std::fs::remove_file(prefix_dir.join(f)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Whether a launch on `runtime` gets NVAPI and DLSS, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvapiStatus {
    /// Everything DLSS needs is in place: NVAPI will be enabled and the
    /// driver's NGX DLLs were found. What a host checks before offering a
    /// DLSS or DLAA option.
    pub available: bool,
    /// NVAPI will be enabled at launch (true even when NGX is missing).
    pub nvapi: bool,
    /// The driver's Wine DLL directory, when found.
    pub ngx_dir: Option<PathBuf>,
    /// A sentence for the user: what was found, or what is missing.
    pub reason: String,
}

/// [`status_on`] for this machine.
pub fn status(runtime: &Path) -> NvapiStatus {
    status_on(&Host::real(), runtime)
}

/// Whether a launch on `runtime` (default options) gets NVAPI and DLSS on
/// `host`, and why not.
pub fn status_on(host: &Host, runtime: &Path) -> NvapiStatus {
    let no = |reason: String| NvapiStatus {
        available: false,
        nvapi: false,
        ngx_dir: None,
        reason,
    };
    match detect_gpu(host) {
        Gpu::None => return no("no NVIDIA GPU found".to_string()),
        Gpu::NvidiaWithoutDriver => {
            return no(
                "an NVIDIA GPU is present but the NVIDIA driver is not loaded \
                 (NVAPI and DLSS need NVIDIA's own driver, not nouveau or nova)"
                    .to_string(),
            )
        }
        Gpu::Nvidia(_) => {}
    }
    if !runtime_nvapi64(runtime).is_file() {
        return no(format!(
            "the Proton runtime has no DXVK-NVAPI ({} is missing)",
            runtime_nvapi64(runtime).display()
        ));
    }
    match find_ngx_dir(host) {
        Some(d) => NvapiStatus {
            available: true,
            nvapi: true,
            reason: format!("NVAPI and DLSS available (NGX from {})", d.display()),
            ngx_dir: Some(d),
        },
        None => NvapiStatus {
            available: false,
            nvapi: true,
            ngx_dir: None,
            reason: "the NVIDIA driver's Wine NGX DLLs (nvngx.dll, in nvidia/wine beside \
                     libGLX_nvidia.so.0) were not found, so DLSS cannot load; NVAPI alone \
                     is still enabled"
                .to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vfs-proton-nvapi-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn pci(host: &Path, slot: &str, vendor: &str, class: &str) {
        let d = host.join("sys/bus/pci/devices").join(slot);
        write(&d.join("vendor"), &format!("{vendor}\n"));
        write(&d.join("class"), &format!("{class}\n"));
    }

    fn fake_runtime(base: &Path) -> PathBuf {
        let rt = base.join("rt");
        let nv = rt.join("files/lib/wine/nvapi");
        write(&nv.join("x86_64-windows/nvapi64.dll"), "nvapi64");
        write(&nv.join("x86_64-windows/nvofapi64.dll"), "nvofapi64");
        write(&nv.join("i386-windows/nvapi.dll"), "nvapi32");
        std::fs::create_dir_all(rt.join("files/lib/wine/nvidia-libs/nvml/wine")).unwrap();
        rt
    }

    fn nvidia_host(base: &Path) -> Host {
        let h = base.join("host");
        write(
            &h.join("proc/driver/nvidia/version"),
            "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  610.57.04\nGCC\n",
        );
        for dll in ["_nvngx.dll", "nvngx.dll", "nvngx_dlssg.dll"] {
            write(&h.join("usr/lib/nvidia/wine").join(dll), dll);
        }
        Host { root: h }
    }

    fn fake_prefix(base: &Path) -> PathBuf {
        let p = base.join("pfx");
        std::fs::create_dir_all(p.join("drive_c/windows/system32")).unwrap();
        std::fs::create_dir_all(p.join("drive_c/windows/syswow64")).unwrap();
        p
    }

    #[test]
    fn detects_the_driver_the_module_list_and_a_bare_nvidia_gpu() {
        let b = tmpdir("detect");
        let h = nvidia_host(&b);
        assert_eq!(
            detect_gpu(&h),
            Gpu::Nvidia(Some(
                "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  610.57.04".into()
            ))
        );

        let m = b.join("modules");
        write(
            &m.join("proc/modules"),
            "nvidia_drm 1 0 - Live\nnvidia 2 0 - Live\n",
        );
        assert_eq!(detect_gpu(&Host { root: m }), Gpu::Nvidia(None));

        let n = b.join("nouveau");
        write(
            &n.join("proc/modules"),
            "nouveau 1 0 - Live\nnvidia_wmi_ec 1 0 - Live\n",
        );
        pci(&n, "0000:00:02.0", "0x8086", "0x030000");
        pci(&n, "0000:01:00.1", "0x10de", "0x040300"); // the GPU's audio function
        assert_eq!(detect_gpu(&Host { root: n.clone() }), Gpu::None);
        pci(&n, "0000:01:00.0", "0x10DE", "0x030000");
        assert_eq!(detect_gpu(&Host { root: n }), Gpu::NvidiaWithoutDriver);

        assert_eq!(
            detect_gpu(&Host {
                root: b.join("empty")
            }),
            Gpu::None
        );
    }

    #[cfg(unix)]
    #[test]
    fn ngx_dir_follows_libglx_first_then_the_fixed_places() {
        let b = tmpdir("ngx");
        let h = b.join("host");
        // Debian-style driver install: the library is a symlink into a
        // versioned directory, and nvidia/wine sits beside the real file.
        let real = h.join("usr/lib/x86_64-linux-gnu/nvidia/current");
        write(&real.join("libGLX_nvidia.so.610"), "");
        write(&real.join("nvidia/wine/nvngx.dll"), "");
        std::os::unix::fs::symlink(
            real.join("libGLX_nvidia.so.610"),
            h.join("usr/lib/x86_64-linux-gnu/libGLX_nvidia.so.0"),
        )
        .unwrap();
        write(&h.join("usr/lib/nvidia/wine/nvngx.dll"), "");
        let host = Host { root: h.clone() };
        assert_eq!(
            find_ngx_dir(&host),
            Some(std::fs::canonicalize(&real).unwrap().join("nvidia/wine"))
        );

        std::fs::remove_file(real.join("nvidia/wine/nvngx.dll")).unwrap();
        assert_eq!(find_ngx_dir(&host), Some(h.join("usr/lib/nvidia/wine")));
        std::fs::remove_file(h.join("usr/lib/nvidia/wine/nvngx.dll")).unwrap();
        write(&h.join("usr/lib64/nvidia/wine/nvngx.dll"), "");
        assert_eq!(find_ngx_dir(&host), Some(h.join("usr/lib64/nvidia/wine")));
        std::fs::remove_file(h.join("usr/lib64/nvidia/wine/nvngx.dll")).unwrap();
        assert_eq!(find_ngx_dir(&host), None);
    }

    #[test]
    fn setup_is_none_without_the_driver_or_the_runtime_dlls() {
        let b = tmpdir("none");
        let rt = fake_runtime(&b);
        let plain = Host {
            root: b.join("plain"),
        };
        assert_eq!(setup(&plain, &rt), None);
        let h = nvidia_host(&b);
        assert_eq!(setup(&h, &b.join("no-runtime")), None);
    }

    #[test]
    fn setup_copies_what_proton_copies() {
        let b = tmpdir("plan");
        let rt = fake_runtime(&b);
        let h = nvidia_host(&b);
        let s = setup(&h, &rt).unwrap();
        let to: Vec<_> = s
            .copies
            .iter()
            .map(|c| c.to.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            to,
            [
                "drive_c/windows/system32/nvapi64.dll",
                "drive_c/windows/system32/nvofapi64.dll",
                "drive_c/windows/syswow64/nvapi.dll",
                "drive_c/windows/system32/_nvngx.dll",
                "drive_c/windows/system32/nvngx.dll",
            ]
        );
        let ngx = h.root.join("usr/lib/nvidia/wine");
        assert_eq!(s.ngx_dir.as_deref(), Some(ngx.as_path()));
        assert_eq!(
            s.nvml_dir,
            Some(rt.join("files/lib/wine/nvidia-libs/nvml/wine"))
        );
        let env: std::collections::HashMap<_, _> = s.env(false).into_iter().collect();
        assert_eq!(env["DXVK_ENABLE_NVAPI"], "1");
        assert_eq!(env["NVIDIA_WINE_DLL_DIR"], ngx.to_string_lossy());
        assert_eq!(env["DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS"], NGX_DEBUG_OPTIONS);
        assert!(!s
            .env(true)
            .iter()
            .any(|(k, _)| k == "DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS"));
    }

    #[test]
    fn install_writes_once_and_again_only_what_changed() {
        let b = tmpdir("install");
        let rt = fake_runtime(&b);
        let h = nvidia_host(&b);
        let pfx = fake_prefix(&b);
        let s = setup(&h, &rt).unwrap();
        assert_eq!(s.install(&pfx).unwrap(), 5);
        let sys32 = pfx.join("drive_c/windows/system32");
        assert_eq!(
            std::fs::read_to_string(sys32.join("nvapi64.dll")).unwrap(),
            "nvapi64"
        );
        assert_eq!(
            std::fs::read_to_string(sys32.join("nvngx.dll")).unwrap(),
            "nvngx.dll"
        );
        assert_eq!(
            std::fs::read_to_string(pfx.join("drive_c/windows/syswow64/nvapi.dll")).unwrap(),
            "nvapi32"
        );
        assert!(
            !sys32.join("nvngx_dlssg.dll").exists(),
            "the script copies only two NGX DLLs"
        );
        assert_eq!(s.install(&pfx).unwrap(), 0, "a set-up prefix is left alone");

        // A runtime update with a new nvapi64.dll of the same length, and a
        // stale copy someone edited, are both replaced.
        write(
            &rt.join("files/lib/wine/nvapi/x86_64-windows/nvapi64.dll"),
            "NVAPI64",
        );
        std::fs::write(sys32.join("nvngx.dll"), "old").unwrap();
        assert_eq!(s.install(&pfx).unwrap(), 2);
        assert_eq!(
            std::fs::read_to_string(sys32.join("nvapi64.dll")).unwrap(),
            "NVAPI64"
        );
        assert_eq!(s.install(&pfx).unwrap(), 0);
        let leftovers = std::fs::read_dir(&sys32)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[cfg(unix)]
    #[test]
    fn install_leaves_copies_writable_and_replaces_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let b = tmpdir("perm");
        let rt = fake_runtime(&b);
        let h = nvidia_host(&b);
        let src = rt.join("files/lib/wine/nvapi/x86_64-windows/nvapi64.dll");
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o555)).unwrap();
        let pfx = fake_prefix(&b);
        let sys32 = pfx.join("drive_c/windows/system32");
        std::os::unix::fs::symlink(&src, sys32.join("nvapi64.dll")).unwrap();
        setup(&h, &rt).unwrap().install(&pfx).unwrap();
        let m = std::fs::symlink_metadata(sys32.join("nvapi64.dll")).unwrap();
        assert!(m.is_file(), "a regular file now, not the link");
        assert_eq!(m.permissions().mode() & 0o200, 0o200);
    }

    #[test]
    fn install_skips_a_missing_syswow64() {
        let b = tmpdir("nowow");
        let rt = fake_runtime(&b);
        let h = nvidia_host(&b);
        let pfx = b.join("pfx");
        std::fs::create_dir_all(pfx.join("drive_c/windows/system32")).unwrap();
        assert_eq!(setup(&h, &rt).unwrap().install(&pfx).unwrap(), 4);
        assert!(!pfx.join("drive_c/windows/syswow64").exists());
    }

    #[test]
    fn remove_deletes_the_nvapi_dlls_like_the_script() {
        let b = tmpdir("remove");
        let rt = fake_runtime(&b);
        let pfx = fake_prefix(&b);
        setup(&nvidia_host(&b), &rt).unwrap().install(&pfx).unwrap();
        remove(&pfx).unwrap();
        let w = pfx.join("drive_c/windows");
        assert!(!w.join("system32/nvapi64.dll").exists());
        assert!(!w.join("syswow64/nvapi.dll").exists());
        assert!(
            w.join("system32/nvofapi64.dll").exists(),
            "the script leaves nvofapi64"
        );
        remove(&pfx).unwrap();
    }

    #[test]
    fn status_says_why() {
        let b = tmpdir("status");
        let rt = fake_runtime(&b);
        let h = nvidia_host(&b);
        let s = status_on(&h, &rt);
        assert!(s.available && s.nvapi, "{s:?}");
        assert!(s.reason.contains("available"), "{}", s.reason);

        let s = status_on(
            &Host {
                root: b.join("plain"),
            },
            &rt,
        );
        assert!(!s.available && !s.nvapi);
        assert_eq!(s.reason, "no NVIDIA GPU found");

        let s = status_on(&h, &b.join("no-runtime"));
        assert!(
            !s.available && s.reason.contains("DXVK-NVAPI"),
            "{}",
            s.reason
        );

        let n = b.join("nouveau");
        pci(&n, "0000:01:00.0", "0x10de", "0x030000");
        let s = status_on(&Host { root: n }, &rt);
        assert!(
            !s.available && s.reason.contains("driver is not loaded"),
            "{}",
            s.reason
        );

        std::fs::remove_file(h.root.join("usr/lib/nvidia/wine/nvngx.dll")).unwrap();
        let s = status_on(&h, &rt);
        assert!(!s.available && s.nvapi && s.ngx_dir.is_none(), "{s:?}");
        assert!(s.reason.contains("nvngx.dll"), "{}", s.reason);
    }
}
