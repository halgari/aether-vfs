//! The injector's failure report.

use std::path::{Path, PathBuf};

/// Where the injector reports why it failed: the ready file's path plus
/// [`vfs_env::INJECTOR_ERROR_SUFFIX`].
pub fn injector_error_path(ready_file: &Path) -> PathBuf {
    let mut s = ready_file.as_os_str().to_owned();
    s.push(vfs_env::INJECTOR_ERROR_SUFFIX);
    PathBuf::from(s)
}

/// Where the shim lists the child processes it killed because it could not
/// inject them: the ready file's path plus [`vfs_env::CHILD_REFUSED_SUFFIX`].
pub fn child_refused_path(ready_file: &Path) -> PathBuf {
    let mut s = ready_file.as_os_str().to_owned();
    s.push(vfs_env::CHILD_REFUSED_SUFFIX);
    PathBuf::from(s)
}

/// The lines of the child-refused file beside `ready_file` (`<image> <reason>`
/// each), empty when none was written.
pub fn read_child_refusals(ready_file: &Path) -> Vec<String> {
    std::fs::read_to_string(child_refused_path(ready_file))
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// One refusal line as a sentence for an error or a launch note.
pub fn describe_child_refusal(line: &str) -> String {
    format!(
        "a child process was refused and killed because the shim could not inject it \
         (`<image> <reason>`): {line}"
    )
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
            "the shim did not report ready within {secs} s, so the target was killed before it \
             ran — it is hung or still starting; raise the ready timeout if a cold prefix is \
             this slow"
        );
    }
    if let Some(why) = raw.strip_prefix(vfs_env::INJECTOR_FUSE_FAILED_PREFIX) {
        return format!(
            "the shim could not attach to the director, so the target was killed before it ran: \
             {why}"
        );
    }
    if let Some(why) = raw.strip_prefix(vfs_env::INJECTOR_BOOTSTRAP_FAILED_PREFIX) {
        return format!(
            "the shim could not bootstrap, so the target was killed before it ran: {why}"
        );
    }
    format!(
        "injection failed: {}",
        raw.strip_prefix(vfs_env::INJECTOR_FAILED_PREFIX)
            .unwrap_or(raw)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injector_reports_are_described() {
        let dll = describe_injector_error("target-exited:0xc0000135\n");
        assert!(
            dll.contains("0xc0000135") && dll.contains("STATUS_DLL_NOT_FOUND"),
            "{dll}"
        );
        let other = describe_injector_error("target-exited:0x1");
        assert!(
            other.contains("0x1") && other.contains("before the shim reported ready"),
            "{other}"
        );
        let t = describe_injector_error("ready-timeout:180");
        assert!(t.contains("180 s"), "{t}");
        let f = describe_injector_error("fuse-failed:no ring");
        assert!(f.contains("attach to the director") && f.contains("no ring"), "{f}");
        let b = describe_injector_error("bootstrap-failed:shim config version 3");
        assert!(b.contains("bootstrap") && b.contains("version 3"), "{b}");
        assert_eq!(
            describe_injector_error("inject:CreateProcess"),
            "injection failed: CreateProcess"
        );
        assert_eq!(
            injector_error_path(Path::new("/s/ready.flag")),
            Path::new("/s/ready.flag.injector-error")
        );
    }

    #[test]
    fn child_refusals_are_read_beside_the_ready_file_and_absent_means_none() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("vfs-proton-refused-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ready = dir.join("ready.flag");
        assert!(read_child_refusals(&ready).is_empty());
        std::fs::write(child_refused_path(&ready), "C:\\a b\\x.exe ready-timeout\n\ny.exe child-32bit\n")
            .unwrap();
        assert_eq!(
            read_child_refusals(&ready),
            ["C:\\a b\\x.exe ready-timeout", "y.exe child-32bit"]
        );
        assert!(child_refused_path(&ready).to_string_lossy().ends_with(".child-refused"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
