//! A launch's own Wine processes, found from the host.
//!
//! In a prefix the launch has to itself, "the launch" is everything in the
//! prefix, and `wineserver` answers for it (`-w` to wait, `-k` to stop). In a
//! prefix it **shares** — with a Steam client a SteamStub-wrapped game must
//! find running beside it — that would wait on Steam and stop it. There the
//! launch is the processes it is running, picked out by two things the host
//! can see of a Wine process:
//!
//! - its **image**: Wine rewrites a process's first argument to its Windows
//!   image path, so the launch's own programs read `C:\Games\X\game.exe` —
//!   under one of the session's locations (its roots, `C:\vfs-session`).
//!   Steam's do not.
//! - its **working directory**: inside the prefix, or inside one of the
//!   session's own host directories (a root's location links to it). This is
//!   what tells the launch's `C:\Games\X\game.exe` from a program of the same
//!   name in another prefix.

use std::path::{Path, PathBuf};

/// What identifies a launch's processes: see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchProcesses {
    /// Windows locations whose programs are the launch's (`C:\Games\X`),
    /// compared ASCII case-insensitively as directory prefixes.
    pub locations: Vec<String>,
    /// Host directories a process of the launch works in: the prefix and the
    /// session's own directories. Canonicalised when set.
    pub dirs: Vec<PathBuf>,
}

impl LaunchProcesses {
    /// `locations` and `dirs`, the directories canonicalised (one that does
    /// not resolve is kept as given).
    pub fn new(locations: Vec<String>, dirs: Vec<PathBuf>) -> Self {
        let dirs = dirs
            .into_iter()
            .map(|d| std::fs::canonicalize(&d).unwrap_or(d))
            .collect();
        LaunchProcesses { locations, dirs }
    }

    /// Whether a process whose first argument is `argv0` and whose working
    /// directory is `cwd` is one of the launch's.
    pub fn owns(&self, argv0: &str, cwd: &Path) -> bool {
        let image = argv0.trim().trim_start_matches('"');
        let under_location = self.locations.iter().any(|loc| {
            let loc = loc.trim_end_matches('\\');
            image.len() > loc.len()
                && image.as_bytes()[loc.len()] == b'\\'
                && image[..loc.len()].eq_ignore_ascii_case(loc)
        });
        under_location && self.dirs.iter().any(|d| cwd.starts_with(d))
    }

    /// The pids of the launch's processes running now.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn running(&self) -> Vec<u32> {
        let me = std::process::id();
        vfs_unix::all_pids()
            .into_iter()
            .filter(|&pid| pid != me)
            .filter(
                |&pid| match (vfs_unix::process_argv0(pid), vfs_unix::process_cwd(pid)) {
                    (Some(argv0), Some(cwd)) => self.owns(&argv0, &cwd),
                    _ => false,
                },
            )
            .collect()
    }

    /// The pids of the launch's processes running now: none, where they
    /// cannot be listed.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn running(&self) -> Vec<u32> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn procs() -> LaunchProcesses {
        LaunchProcesses {
            locations: vec![r"C:\Games\skyrim-se".into(), r"C:\vfs-session\".into()],
            dirs: vec!["/home/u/pfx".into(), "/home/u/launch/root".into()],
        }
    }

    #[test]
    fn a_program_under_a_location_working_in_the_session_is_the_launchs() {
        let p = procs();
        let root = Path::new("/home/u/launch/root");
        assert!(p.owns(r"C:\Games\skyrim-se\SkyrimSE.exe", root));
        assert!(p.owns(r"c:\games\SKYRIM-SE\Data\x.exe", root), "any case");
        assert!(
            p.owns(r#""C:\Games\skyrim-se\a b.exe" -arg"#, root),
            "quoted"
        );
        assert!(p.owns(
            r"C:\vfs-session\root\fixture.exe",
            Path::new("/home/u/pfx/drive_c/users/x")
        ));
    }

    #[test]
    fn steam_another_prefix_and_a_lookalike_location_are_not() {
        let p = procs();
        let root = Path::new("/home/u/launch/root");
        assert!(!p.owns(r"C:\Program Files (x86)\Steam\steam.exe", root));
        assert!(
            !p.owns(
                r"C:\Games\skyrim-se\SkyrimSE.exe",
                Path::new("/home/u/other-pfx")
            ),
            "the same image in another prefix"
        );
        assert!(
            !p.owns(r"C:\Games\skyrim-se-2\SkyrimSE.exe", root),
            "a lookalike"
        );
        assert!(!p.owns(r"C:\Games\skyrim-se", root), "the directory itself");
    }
}
