//! NVAPI: detect.

use std::path::PathBuf;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvapi::test_support::*;

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
}
