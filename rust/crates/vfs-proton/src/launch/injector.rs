//! The injector's failure report.

use std::path::{Path, PathBuf};

/// Where the injector reports why it failed: the ready file's path plus
/// [`vfs_env::INJECTOR_ERROR_SUFFIX`].
pub fn injector_error_path(ready_file: &Path) -> PathBuf {
    let mut s = ready_file.as_os_str().to_owned();
    s.push(vfs_env::INJECTOR_ERROR_SUFFIX);
    PathBuf::from(s)
}

/// A readable account of the injector's one-line failure report.
pub fn describe_injector_error(raw: &str) -> String {
    let raw = raw.trim();
    if let Some(code) = raw.strip_prefix(vfs_env::INJECTOR_TARGET_EXITED_PREFIX) {
        let hint = match u32::from_str_radix(code.trim_start_matches("0x"), 16) {
            Ok(0xC000_0135) => {
                " (STATUS_DLL_NOT_FOUND: a DLL the program imports is missing — stage it \
                 (stage_also / stage_fallback_dirs), or launch in a Proton-initialized prefix, \
                 which carries the DirectX and Visual C++ redistributables)"
            }
            Ok(0xC000_007B) => {
                " (STATUS_INVALID_IMAGE_FORMAT: an imported DLL is not a PE of the right \
                 architecture)"
            }
            Ok(0xC000_0142) => " (STATUS_DLL_INIT_FAILED: a DLL failed to initialise)",
            _ => "",
        };
        return format!("the target exited with {code}{hint} before the shim reported ready");
    }
    if let Some(secs) = raw.strip_prefix(vfs_env::INJECTOR_READY_TIMEOUT_PREFIX) {
        return format!(
            "the shim did not report ready within {secs} s — the target is hung or still \
             starting; raise the ready timeout if a cold prefix is this slow"
        );
    }
    format!(
        "injection failed: {}",
        raw.strip_prefix(vfs_env::INJECTOR_FAILED_PREFIX).unwrap_or(raw)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injector_reports_are_described() {
        let dll = describe_injector_error("target-exited:0xc0000135\n");
        assert!(dll.contains("0xc0000135") && dll.contains("STATUS_DLL_NOT_FOUND"), "{dll}");
        let other = describe_injector_error("target-exited:0x1");
        assert!(other.contains("0x1") && other.contains("before the shim reported ready"), "{other}");
        let t = describe_injector_error("ready-timeout:180");
        assert!(t.contains("180 s"), "{t}");
        assert_eq!(describe_injector_error("inject:CreateProcess"), "injection failed: CreateProcess");
        assert_eq!(
            injector_error_path(Path::new("/s/ready.flag")),
            Path::new("/s/ready.flag.injector-error")
        );
    }
}
