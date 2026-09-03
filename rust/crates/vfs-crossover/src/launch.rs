//! Running a [`WineLaunch`] under CrossOver.
//!
//! The Linux sibling is [`vfs_proton::launch`], and the two differ in exactly
//! two places: which program is executed, and how the prefix is named. The
//! injector's argv and the transport environment are *taken from* that module
//! rather than restated here, because those are the two things where a
//! divergence is silent — a permuted argv still starts a process, and a
//! defaulted `VFS_RING_BYTES` still attaches to the ring.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use vfs_proton::{LaunchError, WineLaunch};

use crate::runtime::Runtime;

/// CrossOver's `wine` — a Perl wrapper, not the loader itself.
///
/// The wrapper is the supported entry point: it sets `WINELOADER`, the
/// library paths and the bottle's environment before exec'ing the Mach-O
/// loader. Driving `bin/wineloader` directly would mean reproducing all of
/// that and re-reproducing it whenever CrossOver changes.
pub fn wine_binary(runtime: &Runtime) -> PathBuf {
    runtime.dir.join("bin").join("wine")
}

/// The program and argv: `wine --bottle <prefix> <injector> <target> …`.
///
/// The tail is [`vfs_proton::command_line`]'s argv verbatim. Only the
/// `--bottle` pair is added, and it must come *before* the program being run,
/// because the wrapper stops parsing its own options at the first
/// non-option argument.
pub fn command_line(runtime: &Runtime, l: &WineLaunch) -> (PathBuf, Vec<String>) {
    let (_linux_prog, injector_argv) = vfs_proton::command_line(l);
    let mut argv = vec!["--bottle".to_string(), path_string(&l.prefix)];
    argv.extend(injector_argv);
    (wine_binary(runtime), argv)
}

/// The environment for a CrossOver launch.
///
/// The `VFS_*` half is [`vfs_proton::vfs_env_block`] — shared, because that is
/// the handshake the shim reads and the place a divergence goes unnoticed.
/// The Wine half is deliberately *not* Proton's:
///
/// - **No `WINEPREFIX`.** CrossOver resolves the prefix from `--bottle` and
///   overwrites `WINEPREFIX` itself while doing so. Setting it here would be
///   a value that looks authoritative and is ignored.
/// - **No `PROTONPATH`.** There is no Proton.
/// - **`CX_ROOT`** instead, which the wrapper needs to find its own Perl
///   modules when it is spawned from a process that did not come from the app
///   bundle. Without it: "Can't locate CXLog.pm in @INC".
pub fn launch_env(runtime: &Runtime, l: &WineLaunch) -> BTreeMap<String, String> {
    let mut env = vfs_proton::vfs_env_block(l);
    env.insert("CX_ROOT".to_string(), path_string(&runtime.dir));
    // Mono and Gecko prompts would otherwise block a launch on a fresh prefix.
    env.insert(
        "WINEDLLOVERRIDES".to_string(),
        "mscoree=d;mshtml=d".to_string(),
    );
    env.insert("WINEDEBUG".to_string(), "-all".to_string());
    env
}

/// Spawns the launch, waits for it, and returns the target's exit code.
///
/// The geometry pre-flight is [`vfs_proton::check_geometry`], run for the same
/// reason it is run on Linux: a child that maps too little of the ring
/// attaches cleanly and fails only once a bulk read lands outside its view.
/// There is no `verify_ge` counterpart — see [`crate::runtime`] for why the
/// polarity of that check does not carry over.
pub fn run(runtime: &Runtime, l: &WineLaunch) -> Result<i32, LaunchError> {
    vfs_proton::check_geometry(l)?;

    let (prog, argv) = command_line(runtime, l);
    let mut cmd = std::process::Command::new(&prog);
    cmd.args(&argv).envs(launch_env(runtime, l));
    for stale in vfs_proton::STALE_TRANSPORT_VARS {
        cmd.env_remove(stale);
    }
    let status = cmd
        .status()
        .map_err(|e| LaunchError::Spawn(format!("{}: {e}", prog.display())))?;

    match status.code() {
        // The injector's own "the target never ran" exits, and the reason
        // `Ok(3)` must not be reported as the target's exit code.
        Some(code @ (2 | 3)) => Err(LaunchError::NonZeroWine(code)),
        Some(code) => Ok(code),
        None => Err(LaunchError::Spawn(format!(
            "{} exited without a code (signalled): {status}",
            prog.display()
        ))),
    }
}

fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch() -> WineLaunch {
        WineLaunch {
            runtime: PathBuf::from("/unused-on-macos"),
            prefix: PathBuf::from("/tmp/sessions/s1/prefix"),
            injector: PathBuf::from("/opt/vfs/vfs-injector.exe"),
            shim_dll: PathBuf::from("/opt/vfs/vfs_shim_dll.dll"),
            payload_dll: PathBuf::from("/opt/vfs/vfs_payload.dll"),
            target: r"C:\vfs-session\root\game.exe".to_string(),
            config_file: PathBuf::from("/tmp/sessions/s1/state/shim.cfg"),
            ready_file: PathBuf::from("/tmp/sessions/s1/state/ready.flag"),
            ring_path: PathBuf::from(r"C:\vfs-session\state\ring.bin"),
            ring_bytes: 33_751_040,
            arena_offset: 132_136,
            arena_len: 33_554_432,
            payload_cap: 4096,
            virtual_dir: r"C:\vfs-session\root".to_string(),
            args: vec!["--windowed".to_string()],
        }
    }

    fn runtime() -> Runtime {
        Runtime {
            dir: PathBuf::from("/Applications/CrossOver.app/Contents/SharedSupport/CrossOver"),
            version: None,
        }
    }

    /// `--bottle <prefix>` must lead, and the injector's five positionals must
    /// follow in the order the injector parses them. Both halves of that are
    /// silent when wrong: options after the program name are passed to the
    /// program, and a permuted positional list still starts a process.
    #[test]
    fn the_bottle_precedes_the_injector_and_the_positionals_keep_their_order() {
        let l = launch();
        let (prog, argv) = command_line(&runtime(), &l);
        assert!(prog.ends_with("bin/wine"), "got {}", prog.display());
        assert_eq!(argv[0], "--bottle");
        assert_eq!(argv[1], "/tmp/sessions/s1/prefix");
        assert_eq!(argv[2], "/opt/vfs/vfs-injector.exe");
        assert_eq!(argv[3], l.target);
        assert_eq!(argv[4], "/opt/vfs/vfs_shim_dll.dll");
        assert_eq!(argv[5], "/opt/vfs/vfs_payload.dll");
        assert_eq!(argv[6], "/tmp/sessions/s1/state/shim.cfg");
        assert_eq!(argv[7], "/tmp/sessions/s1/state/ready.flag");
        assert_eq!(argv[8], "--");
        assert_eq!(argv[9], "--windowed");
    }

    /// The argv tail is `vfs-proton`'s, not a copy. If that contract ever
    /// changes, this fails rather than letting the two hosts drift into
    /// disagreeing about how the injector is called.
    #[test]
    fn the_injector_argv_is_shared_with_the_proton_host() {
        let l = launch();
        let (_, proton_argv) = vfs_proton::command_line(&l);
        let (_, argv) = command_line(&runtime(), &l);
        assert_eq!(&argv[2..], &proton_argv[..]);
    }

    /// The geometry the shim must not guess travels here, and the Wine-side
    /// names that mean nothing to CrossOver do not.
    #[test]
    fn the_env_carries_the_ring_geometry_and_no_proton_names() {
        let l = launch();
        let env = launch_env(&runtime(), &l);
        assert_eq!(
            env.get(vfs_env::RING_BYTES).map(String::as_str),
            Some("33751040")
        );
        assert_eq!(
            env.get(vfs_env::ARENA_LEN).map(String::as_str),
            Some("33554432")
        );
        assert_eq!(
            env.get(vfs_env::ARENA_OFFSET).map(String::as_str),
            Some("132136")
        );
        assert_eq!(
            env.get(vfs_env::RING_PAYLOAD_CAP).map(String::as_str),
            Some("4096")
        );
        assert_eq!(
            env.get(vfs_env::VIRTUAL_DIR).map(String::as_str),
            Some(l.virtual_dir.as_str())
        );
        assert_eq!(
            env.get(vfs_env::RING_PATH).map(String::as_str),
            Some(r"C:\vfs-session\state\ring.bin"),
            "the ring travels as the shim sees it, not as a host path"
        );
        assert!(
            !env.contains_key("PROTONPATH"),
            "there is no Proton here; a PROTONPATH would be a lie in a diagnostic"
        );
        assert!(
            !env.contains_key("WINEPREFIX"),
            "CrossOver resolves the prefix from --bottle and overwrites WINEPREFIX itself, \
             so setting it would look authoritative and do nothing"
        );
        assert!(
            env.contains_key("CX_ROOT"),
            "the wrapper cannot find its own modules without it"
        );
    }
}
