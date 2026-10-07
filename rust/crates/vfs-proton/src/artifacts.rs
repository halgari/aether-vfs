//! The Windows build artefacts a Proton launch and its tests need, by file
//! name.
//!
//! They are a Windows cross-build (`bin/build-windows`), not part of any
//! Linux build, so every piece of code that has to find them used to carry its
//! own list: the launch, the test support, Haskill's locator and the script
//! itself. This is the one list. The script cannot read Rust, so a test here
//! reads the script's `ARTIFACTS=(…)` array and asserts it equals
//! [`WINDOWS_ARTIFACTS`].

/// The injector: the Windows program Wine runs to start the image and load
/// the shim into it.
pub const INJECTOR: &str = "vfs-injector.exe";
/// The shim DLL, which remaps the program's I/O to the director.
pub const SHIM_DLL: &str = "vfs_shim_dll.dll";
/// The payload DLL the injector places in the program.
pub const PAYLOAD_DLL: &str = "vfs_payload.dll";

/// What a Proton launch itself needs: the injector, the shim and the payload.
pub const LAUNCH: [&str; 3] = [INJECTOR, SHIM_DLL, PAYLOAD_DLL];

/// The Windows test programs the Proton end-to-end tests run.
pub const FIXTURE_READ: &str = "vfs-fixture-read.exe";
pub const FIXTURE_STEAM: &str = "vfs-fixture-steam.exe";
pub const FIXTURE_NVAPI: &str = "vfs-fixture-nvapi.exe";
pub const FIXTURE_REGISTRY: &str = "vfs-fixture-registry.exe";

/// Every artefact `bin/build-windows` produces, in the script's order: the
/// launch's three, then the fixtures.
pub const WINDOWS_ARTIFACTS: [&str; 7] = [
    INJECTOR,
    SHIM_DLL,
    PAYLOAD_DLL,
    FIXTURE_READ,
    FIXTURE_STEAM,
    FIXTURE_NVAPI,
    FIXTURE_REGISTRY,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The names in `bin/build-windows`'s `ARTIFACTS=( … )` array.
    fn script_list() -> Vec<String> {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../bin/build-windows");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        text.lines()
            .skip_while(|l| l.trim() != "ARTIFACTS=(")
            .skip(1)
            .take_while(|l| l.trim() != ")")
            .map(|l| l.trim().to_string())
            .collect()
    }

    #[test]
    fn the_build_script_builds_exactly_the_artifacts_listed_here() {
        assert_eq!(
            script_list(),
            WINDOWS_ARTIFACTS,
            "bin/build-windows ARTIFACTS"
        );
    }

    #[test]
    fn the_launch_artifacts_are_the_first_of_the_list() {
        assert_eq!(WINDOWS_ARTIFACTS[..3], LAUNCH);
    }
}
