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
//!   without loading the driver into this process (in the usual library
//!   directories and those the linker is configured for), and in fixed
//!   locations only when no `libGLX_nvidia.so.0` exists.
//! - The session sets `DXVK_ENABLE_NVAPI=1` (otherwise DXVK reports an NVIDIA
//!   GPU as AMD and NVAPI finds no adapter) and, unless already set,
//!   `DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS=DLSSIndicator=0,DLSSGIndicator=0,`.
//! - With the NVIDIA kernel module loaded it turns on wine-nvml, which puts
//!   `lib/wine/nvidia-libs/nvml/wine` first in `WINEDLLPATH`.
//!
//! The script does the copies whatever the GPU is; this crate does them only
//! when an NVIDIA driver is loaded, so a launch on any other machine is
//! unchanged. Turning it off on an NVIDIA machine ([`remove`]) deletes the two
//! NVAPI DLLs as the script does when NVAPI is disabled. Like the script,
//! nothing is cleaned out of a prefix when the NVIDIA driver later goes away
//! (another GPU, or nouveau): without `DXVK_ENABLE_NVAPI` the leftover DLLs
//! find no adapter and do nothing.
//!
//! [`status`] is what a host asks before offering DLSS: NVAPI alone is not
//! enough, DLSS also needs an RTX GPU and the driver's NGX DLLs.

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
/// directory's `nvidia/wine` is where the script finds the NGX DLLs; the
/// directories `/etc/ld.so.conf` and `/etc/ld.so.conf.d/*.conf` list are
/// searched after these.
const GLX_LIB_DIRS: [&str; 7] = [
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib64",
    "/usr/lib",
    "/lib/x86_64-linux-gnu",
    "/lib64",
    "/usr/local/lib",
    // NixOS.
    "/run/opengl-driver/lib",
];
/// Where distributions put the driver's Wine DLLs, looked at only when no
/// `libGLX_nvidia.so.0` was found at all.
const NGX_DIRS: [&str; 3] = [
    "/usr/lib/nvidia/wine",
    "/usr/lib64/nvidia/wine",
    "/usr/lib/x86_64-linux-gnu/nvidia/wine",
];

/// The directories the dynamic linker's configuration names: every
/// non-comment line of `/etc/ld.so.conf` and `/etc/ld.so.conf.d/*.conf` that
/// is a path (`include` lines are what pull in the `.d` files).
fn ld_conf_dirs(host: &Host) -> Vec<String> {
    let mut files = vec![host.path("/etc/ld.so.conf")];
    if let Ok(rd) = std::fs::read_dir(host.path("/etc/ld.so.conf.d")) {
        let mut confs: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "conf"))
            .collect();
        confs.sort();
        files.extend(confs);
    }
    files
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .flat_map(|t| {
            t.lines()
                .map(|l| l.split('#').next().unwrap_or("").trim().to_string())
                .filter(|l| l.starts_with('/'))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The loaded driver's version (`610.57.04`) from `/proc/driver/nvidia/version`.
fn driver_version(host: &Host) -> Option<String> {
    let v = std::fs::read_to_string(host.path("/proc/driver/nvidia/version")).ok()?;
    let first = v.lines().next()?;
    first
        .split_whitespace()
        .find(|t| t.contains('.') && t.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .map(str::to_string)
}

/// The NVIDIA driver's Wine DLL directory, holding `nvngx.dll` (the script's
/// own test).
///
/// As the script resolves it: `nvidia/wine` beside the real file of
/// `libGLX_nvidia.so.0`, looked for in [`GLX_LIB_DIRS`] and the linker's
/// configured directories. When several are found, those whose real name
/// carries the loaded driver's version are the only ones considered, so a
/// leftover driver's directory is not paired with the running one. Only when
/// no `libGLX_nvidia.so.0` exists anywhere are the fixed [`NGX_DIRS`] tried.
/// The result is a path on this host (under [`Host::root`]).
pub fn find_ngx_dir(host: &Host) -> Option<PathBuf> {
    let mut libs: Vec<PathBuf> = Vec::new();
    let dirs = GLX_LIB_DIRS
        .iter()
        .map(|d| d.to_string())
        .chain(ld_conf_dirs(host));
    for d in dirs {
        let lib = host.path(&d).join("libGLX_nvidia.so.0");
        if let Ok(real) = std::fs::canonicalize(&lib) {
            if !libs.contains(&real) {
                libs.push(real);
            }
        }
    }
    let ngx = |lib: &PathBuf| {
        let d = lib.parent()?.join("nvidia").join("wine");
        d.join("nvngx.dll").is_file().then_some(d)
    };
    if libs.is_empty() {
        return NGX_DIRS
            .iter()
            .map(|d| host.path(d))
            .find(|d| d.join("nvngx.dll").is_file());
    }
    if let Some(ver) = driver_version(host) {
        let matching: Vec<&PathBuf> = libs
            .iter()
            .filter(|l| l.to_string_lossy().ends_with(&format!(".so.{ver}")))
            .collect();
        if !matching.is_empty() {
            return matching.into_iter().find_map(ngx);
        }
    }
    libs.iter().find_map(ngx)
}

/// One NVIDIA GPU, as `/proc/driver/nvidia/gpus/*/information` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuModel {
    /// The `Model:` line, e.g. `NVIDIA GeForce RTX 5090`.
    pub name: String,
    /// An RTX GPU (tensor cores): what DLSS runs on. GeForce GTX, GTX 16
    /// (Turing without tensor cores) and compute cards without RTX in the
    /// name are not.
    pub rtx: bool,
    /// The RTX generation by product series: 20 (Turing: GeForce RTX 20,
    /// Quadro RTX, TITAN RTX), 30 (Ampere: GeForce RTX 30, RTX A-series), 40
    /// (Ada: GeForce RTX 40, RTX … Ada Generation), 50 (Blackwell: GeForce
    /// RTX 50, RTX PRO). `None` when not RTX or not recognised. DLSS frame
    /// generation needs 40 or later, multi-frame generation 50.
    pub rtx_generation: Option<u32>,
}

impl GpuModel {
    /// Classifies a driver model name.
    pub fn from_name(name: &str) -> GpuModel {
        let up = name.to_ascii_uppercase();
        let toks: Vec<&str> = up.split_whitespace().collect();
        let rtx_at = toks.iter().position(|t| *t == "RTX");
        let generation = rtx_at.and_then(|i| {
            let next = toks.get(i + 1).copied().unwrap_or("");
            if toks.contains(&"BLACKWELL") || next == "PRO" {
                Some(50)
            } else if toks.contains(&"ADA") {
                Some(40)
            } else if i > 0 && matches!(toks[i - 1], "QUADRO" | "TITAN") {
                Some(20)
            } else if next.len() > 1
                && next.starts_with('A')
                && next[1..].chars().all(|c| c.is_ascii_digit())
            {
                Some(30)
            } else if next.len() == 4 && next.chars().all(|c| c.is_ascii_digit()) {
                next.parse::<u32>().ok().map(|n| n / 100)
            } else {
                None
            }
        });
        GpuModel {
            name: name.to_string(),
            rtx: rtx_at.is_some(),
            rtx_generation: generation,
        }
    }
}

/// Every NVIDIA GPU the driver reports, in bus order.
pub fn gpu_models(host: &Host) -> Vec<GpuModel> {
    let Ok(rd) = std::fs::read_dir(host.path("/proc/driver/nvidia/gpus")) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    dirs.sort();
    dirs.iter()
        .filter_map(|d| std::fs::read_to_string(d.join("information")).ok())
        .filter_map(|info| {
            info.lines()
                .find_map(|l| l.strip_prefix("Model:"))
                .map(|m| GpuModel::from_name(m.trim()))
        })
        .collect()
}

/// Whether a `PROTON_DISABLE_NVAPI` value turns NVAPI off: set, non-empty and
/// not `0`, as the script's `nonzero` reads it.
pub fn disabled_by(proton_disable_nvapi: Option<&str>) -> bool {
    proton_disable_nvapi.is_some_and(|v| !v.is_empty() && v != "0")
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
    /// exist (a prefix with no `syswow64`) is skipped. A file that cannot be
    /// copied does not stop the others: NVAPI and NGX are optional for the
    /// program, and the script only logs these failures too.
    pub fn install(&self, prefix_dir: &Path) -> Installed {
        let mut out = Installed::default();
        for c in &self.copies {
            let dst = prefix_dir.join(&c.to);
            let Some(dir) = dst.parent().filter(|d| d.is_dir()) else {
                continue;
            };
            let tmp = dir.join(format!(
                ".{}.aether-nvapi.tmp",
                dst.file_name().unwrap_or_default().to_string_lossy()
            ));
            let res = (|| {
                if same_contents(&c.from, &dst)? {
                    return Ok(false);
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::copy(&c.from, &tmp)?;
                    let mut perm = std::fs::metadata(&tmp)?.permissions();
                    perm.set_mode(perm.mode() | 0o220);
                    std::fs::set_permissions(&tmp, perm)?;
                }
                // A fresh file carries no read-only attribute, unlike
                // `fs::copy`, which copies the source's.
                #[cfg(not(unix))]
                {
                    let mut from = std::fs::File::open(&c.from)?;
                    let mut to = std::fs::File::create(&tmp)?;
                    io::copy(&mut from, &mut to)?;
                }
                std::fs::rename(&tmp, &dst)?;
                Ok::<_, io::Error>(true)
            })();
            match res {
                Ok(true) => out.written += 1,
                Ok(false) => {}
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    out.failed
                        .push(format!("{} -> {}: {e}", c.from.display(), dst.display()));
                }
            }
        }
        out
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

/// What [`Setup::install`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Installed {
    /// Files written: `0` on a prefix already set up.
    pub written: usize,
    /// One line per file that could not be copied, naming it and why.
    pub failed: Vec<String>,
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

/// Whether something is available, and a sentence saying why or why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub available: bool,
    pub reason: String,
}

/// Whether a launch on `runtime` gets NVAPI and DLSS, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvapiStatus {
    /// NVAPI will be enabled at launch: the NVIDIA driver is loaded, the
    /// runtime has DXVK-NVAPI, and `PROTON_DISABLE_NVAPI` does not say no.
    pub nvapi: Capability,
    /// DLSS (and DLAA) can run: NVAPI as above, an RTX GPU, and the driver's
    /// Wine NGX DLLs found. What a host checks before offering a DLSS or DLAA
    /// option; [`GpuModel::rtx_generation`] says which DLSS features fit.
    pub dlss: Capability,
    /// The GPU DLSS would run on: the first RTX GPU the driver reports, else
    /// its first GPU. `None` when the driver reports no model.
    pub gpu: Option<GpuModel>,
    /// The driver's Wine DLL directory, when found.
    pub ngx_dir: Option<PathBuf>,
}

/// [`status_on`] for this machine, with this process's
/// `PROTON_DISABLE_NVAPI`.
pub fn status(runtime: &Path) -> NvapiStatus {
    let env = std::env::var("PROTON_DISABLE_NVAPI").ok();
    status_on(&Host::real(), runtime, disabled_by(env.as_deref()))
}

/// Whether a launch on `runtime` (default options) gets NVAPI and DLSS on
/// `host`, and why not. `disabled`: `PROTON_DISABLE_NVAPI` is set
/// ([`disabled_by`]).
pub fn status_on(host: &Host, runtime: &Path, disabled: bool) -> NvapiStatus {
    let models = gpu_models(host);
    let gpu = models.iter().find(|m| m.rtx).or(models.first()).cloned();
    let no = |reason: String| NvapiStatus {
        nvapi: Capability {
            available: false,
            reason: reason.clone(),
        },
        dlss: Capability {
            available: false,
            reason,
        },
        gpu: gpu.clone(),
        ngx_dir: None,
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
    if disabled {
        return no("PROTON_DISABLE_NVAPI is set, so NVAPI and DLSS are off".to_string());
    }
    let nvapi = Capability {
        available: true,
        reason: "NVAPI available".to_string(),
    };
    let ngx_dir = find_ngx_dir(host);
    let dlss = |available: bool, reason: String| Capability { available, reason };
    let dlss = match (&gpu, &ngx_dir) {
        (None, _) => dlss(
            false,
            "the GPU model could not be read from /proc/driver/nvidia/gpus, so DLSS support \
             is unknown; NVAPI alone is enabled"
                .to_string(),
        ),
        (Some(g), _) if !g.rtx => dlss(
            false,
            format!(
                "the {} has no DLSS support (DLSS needs an NVIDIA RTX GPU); NVAPI alone is \
                 enabled",
                g.name
            ),
        ),
        (Some(_), Some(d)) if NGX_DLLS.iter().all(|f| d.join(f).is_file()) => dlss(
            true,
            format!(
                "DLSS available on the {} (NGX from {})",
                gpu.as_ref().map(|g| g.name.as_str()).unwrap_or_default(),
                d.display()
            ),
        ),
        (Some(_), _) => dlss(
            false,
            "the NVIDIA driver's Wine NGX DLLs (_nvngx.dll and nvngx.dll, in nvidia/wine \
             beside libGLX_nvidia.so.0) were not found, so DLSS cannot load; NVAPI alone is \
             enabled"
                .to_string(),
        ),
    };
    NvapiStatus {
        nvapi,
        dlss,
        gpu,
        ngx_dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = crate::test_tmp::dir().join(format!("vfs-proton-nvapi-{}-{tag}", std::process::id()));
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
        gpu_info(&h, "0000:01:00.0", "NVIDIA GeForce RTX 5090");
        Host { root: h }
    }

    /// A `/proc/driver/nvidia/gpus/<bus>/information` as the driver writes it.
    fn gpu_info(host: &Path, bus: &str, model: &str) {
        write(
            &host
                .join("proc/driver/nvidia/gpus")
                .join(bus)
                .join("information"),
            &format!("Model: \t\t {model}\nIRQ:   \t\t 76\nBus Location: \t {bus}\n"),
        );
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
    fn ngx_dir_follows_libglx_and_uses_the_fixed_places_only_without_one() {
        let b = tmpdir("ngx");
        let h = b.join("host");
        let host = Host { root: h.clone() };
        // No libGLX anywhere: the fixed places, in order.
        write(&h.join("usr/lib64/nvidia/wine/nvngx.dll"), "");
        assert_eq!(find_ngx_dir(&host), Some(h.join("usr/lib64/nvidia/wine")));
        write(&h.join("usr/lib/nvidia/wine/nvngx.dll"), "");
        assert_eq!(find_ngx_dir(&host), Some(h.join("usr/lib/nvidia/wine")));

        // Debian-style driver install: the library is a symlink into a
        // versioned directory, and nvidia/wine sits beside the real file.
        let real = h.join("usr/lib/x86_64-linux-gnu/nvidia/current");
        write(&real.join("libGLX_nvidia.so.610.57.04"), "");
        write(&real.join("nvidia/wine/nvngx.dll"), "");
        std::os::unix::fs::symlink(
            real.join("libGLX_nvidia.so.610.57.04"),
            h.join("usr/lib/x86_64-linux-gnu/libGLX_nvidia.so.0"),
        )
        .unwrap();
        let wine = std::fs::canonicalize(&real).unwrap().join("nvidia/wine");
        assert_eq!(find_ngx_dir(&host), Some(wine.clone()));

        // A libGLX without nvidia/wine beside it: not the fixed places, which
        // may belong to another driver.
        std::fs::remove_file(real.join("nvidia/wine/nvngx.dll")).unwrap();
        assert_eq!(find_ngx_dir(&host), None);
        write(&real.join("nvidia/wine/nvngx.dll"), "");

        // A leftover driver in a directory the linker is configured for,
        // listed after: with the loaded version known, only its library counts.
        write(
            &h.join("etc/ld.so.conf"),
            "include /etc/ld.so.conf.d/*.conf\n",
        );
        write(
            &h.join("etc/ld.so.conf.d/old.conf"),
            "# old driver\n/opt/old\n",
        );
        let old = h.join("opt/old");
        write(&old.join("libGLX_nvidia.so.550.1"), "");
        write(&old.join("nvidia/wine/nvngx.dll"), "");
        std::os::unix::fs::symlink(
            old.join("libGLX_nvidia.so.550.1"),
            old.join("libGLX_nvidia.so.0"),
        )
        .unwrap();
        write(
            &h.join("proc/driver/nvidia/version"),
            "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  550.1  Release\n",
        );
        assert_eq!(
            find_ngx_dir(&host),
            Some(std::fs::canonicalize(&old).unwrap().join("nvidia/wine")),
            "the loaded driver's library wins even when listed later"
        );
        write(
            &h.join("proc/driver/nvidia/version"),
            "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  610.57.04  Release\n",
        );
        assert_eq!(find_ngx_dir(&host), Some(wine.clone()));
        // The version-matched library has no NGX: nothing, not the other one.
        std::fs::remove_file(real.join("nvidia/wine/nvngx.dll")).unwrap();
        assert_eq!(find_ngx_dir(&host), None);
        // Version unknown: the first library with NGX.
        std::fs::remove_file(h.join("proc/driver/nvidia/version")).unwrap();
        assert_eq!(
            find_ngx_dir(&host),
            Some(std::fs::canonicalize(&old).unwrap().join("nvidia/wine"))
        );
    }

    #[test]
    fn gpu_models_name_rtx_and_its_generation() {
        let cases: [(&str, bool, Option<u32>); 16] = [
            ("NVIDIA GeForce RTX 5090", true, Some(50)),
            ("NVIDIA GeForce RTX 4070 Laptop GPU", true, Some(40)),
            ("NVIDIA GeForce RTX 3060 Ti", true, Some(30)),
            ("NVIDIA GeForce RTX 2080 SUPER", true, Some(20)),
            ("NVIDIA GeForce RTX 2050", true, Some(20)),
            ("NVIDIA TITAN RTX", true, Some(20)),
            ("Quadro RTX 5000 with Max-Q Design", true, Some(20)),
            ("NVIDIA RTX A6000", true, Some(30)),
            ("NVIDIA RTX A2000 Laptop GPU", true, Some(30)),
            ("NVIDIA RTX 5000 Ada Generation", true, Some(40)),
            (
                "NVIDIA RTX PRO 6000 Blackwell Server Edition",
                true,
                Some(50),
            ),
            ("NVIDIA GeForce GTX 1660 SUPER", false, None),
            ("NVIDIA GeForce GTX 1080 Ti", false, None),
            ("Tesla T4", false, None),
            ("NVIDIA A100-SXM4-80GB", false, None),
            ("NVIDIA RTX Future", true, None),
        ];
        for (name, rtx, generation) in cases {
            let m = GpuModel::from_name(name);
            assert_eq!((m.rtx, m.rtx_generation), (rtx, generation), "{name}");
        }

        let b = tmpdir("models");
        gpu_info(&b, "0000:02:00.0", "NVIDIA GeForce RTX 4090");
        gpu_info(&b, "0000:01:00.0", "NVIDIA GeForce GTX 1080");
        let names: Vec<String> = gpu_models(&Host { root: b })
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(
            names,
            ["NVIDIA GeForce GTX 1080", "NVIDIA GeForce RTX 4090"]
        );
    }

    #[test]
    fn proton_disable_nvapi_reads_like_the_script() {
        assert!(!disabled_by(None));
        assert!(!disabled_by(Some("")));
        assert!(!disabled_by(Some("0")));
        assert!(disabled_by(Some("1")));
        assert!(disabled_by(Some("yes")));
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
        assert_eq!(
            s.install(&pfx),
            Installed {
                written: 5,
                failed: vec![]
            }
        );
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
        assert_eq!(s.install(&pfx).written, 0, "a set-up prefix is left alone");

        // A runtime update with a new nvapi64.dll of the same length, and a
        // stale copy someone edited, are both replaced.
        write(
            &rt.join("files/lib/wine/nvapi/x86_64-windows/nvapi64.dll"),
            "NVAPI64",
        );
        std::fs::write(sys32.join("nvngx.dll"), "old").unwrap();
        assert_eq!(s.install(&pfx).written, 2);
        assert_eq!(
            std::fs::read_to_string(sys32.join("nvapi64.dll")).unwrap(),
            "NVAPI64"
        );
        assert_eq!(s.install(&pfx).written, 0);
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
        assert!(setup(&h, &rt).unwrap().install(&pfx).failed.is_empty());
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
        assert_eq!(setup(&h, &rt).unwrap().install(&pfx).written, 4);
        assert!(!pfx.join("drive_c/windows/syswow64").exists());
    }

    #[test]
    fn remove_deletes_the_nvapi_dlls_like_the_script() {
        let b = tmpdir("remove");
        let rt = fake_runtime(&b);
        let pfx = fake_prefix(&b);
        setup(&nvidia_host(&b), &rt).unwrap().install(&pfx);
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

    #[cfg(unix)]
    #[test]
    fn install_reports_a_file_it_cannot_copy_and_copies_the_rest() {
        let b = tmpdir("fail");
        let rt = fake_runtime(&b);
        let h = nvidia_host(&b);
        let s = setup(&h, &rt).unwrap();
        // The driver package is mid-upgrade: one NGX DLL is gone.
        std::fs::remove_file(h.root.join("usr/lib/nvidia/wine/_nvngx.dll")).unwrap();
        let pfx = fake_prefix(&b);
        let got = s.install(&pfx);
        assert_eq!(got.written, 4);
        assert_eq!(got.failed.len(), 1, "{:?}", got.failed);
        assert!(got.failed[0].contains("_nvngx.dll"), "{:?}", got.failed);
    }

    #[test]
    fn status_says_why() {
        let b = tmpdir("status");
        let rt = fake_runtime(&b);
        let h = nvidia_host(&b);
        let s = status_on(&h, &rt, false);
        assert!(s.nvapi.available && s.dlss.available, "{s:?}");
        assert!(
            s.dlss.reason.contains("NVIDIA GeForce RTX 5090"),
            "{}",
            s.dlss.reason
        );
        assert_eq!(s.gpu.as_ref().and_then(|g| g.rtx_generation), Some(50));

        let s = status_on(&h, &rt, true);
        assert!(!s.nvapi.available && !s.dlss.available);
        assert!(
            s.nvapi.reason.contains("PROTON_DISABLE_NVAPI"),
            "{}",
            s.nvapi.reason
        );

        let s = status_on(
            &Host {
                root: b.join("plain"),
            },
            &rt,
            false,
        );
        assert!(!s.nvapi.available && !s.dlss.available);
        assert_eq!(s.nvapi.reason, "no NVIDIA GPU found");

        let s = status_on(&h, &b.join("no-runtime"), false);
        assert!(
            !s.nvapi.available && s.nvapi.reason.contains("DXVK-NVAPI"),
            "{s:?}"
        );

        let n = b.join("nouveau");
        pci(&n, "0000:01:00.0", "0x10de", "0x030000");
        let s = status_on(&Host { root: n }, &rt, false);
        assert!(
            !s.nvapi.available && s.dlss.reason.contains("driver is not loaded"),
            "{s:?}"
        );

        // A GTX card: NVAPI yes, DLSS no, and the model says why.
        let g = b.join("gtx");
        std::fs::create_dir_all(&g).unwrap();
        let gh = nvidia_host(&g);
        gpu_info(&gh.root, "0000:01:00.0", "NVIDIA GeForce GTX 1660 SUPER");
        let s = status_on(&gh, &rt, false);
        assert!(s.nvapi.available && !s.dlss.available, "{s:?}");
        assert!(
            s.dlss.reason.contains("GTX 1660 SUPER has no DLSS"),
            "{}",
            s.dlss.reason
        );
        assert_eq!(s.gpu.as_ref().map(|g| g.rtx), Some(false));
        // ... with an RTX card beside it, DLSS runs on that one.
        gpu_info(&gh.root, "0000:02:00.0", "NVIDIA RTX A4000");
        let s = status_on(&gh, &rt, false);
        assert!(s.dlss.available, "{s:?}");
        assert_eq!(s.gpu.as_ref().and_then(|g| g.rtx_generation), Some(30));

        // No model readable: DLSS unknown, so not offered.
        std::fs::remove_dir_all(gh.root.join("proc/driver/nvidia/gpus")).unwrap();
        let s = status_on(&gh, &rt, false);
        assert!(
            s.nvapi.available && !s.dlss.available && s.gpu.is_none(),
            "{s:?}"
        );
        assert!(
            s.dlss.reason.contains("could not be read"),
            "{}",
            s.dlss.reason
        );

        // Both NGX DLLs are needed.
        std::fs::remove_file(h.root.join("usr/lib/nvidia/wine/_nvngx.dll")).unwrap();
        let s = status_on(&h, &rt, false);
        assert!(s.nvapi.available && !s.dlss.available, "{s:?}");
        assert!(s.dlss.reason.contains("_nvngx.dll"), "{}", s.dlss.reason);
        std::fs::remove_file(h.root.join("usr/lib/nvidia/wine/nvngx.dll")).unwrap();
        let s = status_on(&h, &rt, false);
        assert!(!s.dlss.available && s.ngx_dir.is_none(), "{s:?}");
    }
}
