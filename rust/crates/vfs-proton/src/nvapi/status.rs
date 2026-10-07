//! NVAPI: status.

use std::path::{Path, PathBuf};

use super::*;

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
            );
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
    use crate::nvapi::test_support::*;

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
