//! NVAPI: install.

use std::io;
use std::path::{Path, PathBuf};

use super::*;

/// `WINEDLLOVERRIDES` entries the `proton` script sets when NVAPI is on.
pub const NVAPI_OVERRIDES: &str = "nvapi64=n;nvofapi64=n;nvapi=n;nvcuda=b";

/// `DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS` unless the host sets its own: no DLSS
/// on-screen indicator.
pub const NGX_DEBUG_OPTIONS: &str = "DLSSIndicator=0,DLSSGIndicator=0,";

/// The NGX DLLs the script copies from the driver's Wine directory.
pub const NGX_DLLS: [&str; 2] = ["_nvngx.dll", "nvngx.dll"];

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
pub(super) fn runtime_nvapi64(runtime: &Path) -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvapi::test_support::*;

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
}
