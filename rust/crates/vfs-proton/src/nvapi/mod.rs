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

mod detect;
mod install;
mod status;

pub use detect::*;
pub use install::*;
pub use status::*;

#[cfg(test)]
mod test_support {
    use super::*;
    use std::path::{Path, PathBuf};

    pub(super) fn tmpdir(tag: &str) -> PathBuf {
        let d = crate::test_tmp::dir().join(format!("vfs-proton-nvapi-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub(super) fn write(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    pub(super) fn pci(host: &Path, slot: &str, vendor: &str, class: &str) {
        let d = host.join("sys/bus/pci/devices").join(slot);
        write(&d.join("vendor"), &format!("{vendor}\n"));
        write(&d.join("class"), &format!("{class}\n"));
    }

    pub(super) fn fake_runtime(base: &Path) -> PathBuf {
        let rt = base.join("rt");
        let nv = rt.join("files/lib/wine/nvapi");
        write(&nv.join("x86_64-windows/nvapi64.dll"), "nvapi64");
        write(&nv.join("x86_64-windows/nvofapi64.dll"), "nvofapi64");
        write(&nv.join("i386-windows/nvapi.dll"), "nvapi32");
        std::fs::create_dir_all(rt.join("files/lib/wine/nvidia-libs/nvml/wine")).unwrap();
        rt
    }

    pub(super) fn nvidia_host(base: &Path) -> Host {
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
    pub(super) fn gpu_info(host: &Path, bus: &str, model: &str) {
        write(
            &host
                .join("proc/driver/nvidia/gpus")
                .join(bus)
                .join("information"),
            &format!("Model: \t\t {model}\nIRQ:   \t\t 76\nBus Location: \t {bus}\n"),
        );
    }

    pub(super) fn fake_prefix(base: &Path) -> PathBuf {
        let p = base.join("pfx");
        std::fs::create_dir_all(p.join("drive_c/windows/system32")).unwrap();
        std::fs::create_dir_all(p.join("drive_c/windows/syswow64")).unwrap();
        p
    }
}
