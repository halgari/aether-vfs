//! Finding build artefacts (fixture executables, the shim DLL) from a test.

use std::path::PathBuf;

/// The cargo profile directory the running test binary was built into:
/// `target/<profile>`, or `target/<triple>/<profile>` for a cross build.
pub fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let dir = exe.parent().unwrap();
    if dir.file_name().and_then(|s| s.to_str()) == Some("deps") {
        dir.parent().unwrap().to_path_buf()
    } else {
        dir.to_path_buf()
    }
}

/// Look for `name` in the profile directory and its `deps/`, then in the
/// workspace's own `target/{debug,release}` (for a test run from an
/// unexpected layout).
pub fn locate(name: &str) -> Option<PathBuf> {
    let profile = profile_dir();
    for cand in [profile.join(name), profile.join("deps").join(name)] {
        if cand.is_file() {
            return Some(cand);
        }
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target");
    for profile in ["debug", "release"] {
        for cand in [
            root.join(profile).join(name),
            root.join(profile).join("deps").join(name),
        ] {
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// [`locate`], panicking with `hint` (what to build first) when it is absent.
pub fn locate_or_panic(name: &str, hint: &str) -> PathBuf {
    locate(name).unwrap_or_else(|| panic!("{name} not found near {:?}; {hint}", profile_dir()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_dir_is_not_the_deps_directory() {
        assert_ne!(
            profile_dir().file_name().and_then(|s| s.to_str()),
            Some("deps")
        );
    }

    #[test]
    fn a_file_beside_the_profile_is_found_and_a_missing_one_is_not() {
        assert!(locate("definitely-not-an-artefact.nope").is_none());
        let dep = profile_dir()
            .join("deps")
            .join(format!("testkit-probe-{}", std::process::id()));
        std::fs::write(&dep, b"x").unwrap();
        let found = locate(dep.file_name().unwrap().to_str().unwrap());
        let _ = std::fs::remove_file(&dep);
        assert_eq!(found, Some(dep));
    }
}
