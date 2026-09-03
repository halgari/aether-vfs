//! `dir-ring-client`: does a Wine-hosted shim **enumerate** a virtual
//! directory?
//!
//! The sibling of `shim-ring-client`, which proves *reads*. Reading and listing
//! are different hooks and fail differently, and the difference is what this
//! binary exists to isolate.
//!
//! # Why it exists
//!
//! Measured 2026-09-02 under CrossOver on macOS: Skyrim launched, the mount
//! served 4,274 opens, and SKSE reported
//!
//! ```text
//! scanning plugin directory C:\vfs-session\root\Data\SKSE\Plugins\
//! dispatch message (0) to plugin listeners
//! no listeners registered
//! ```
//!
//! — zero plugins, out of 176 DLLs the manifest carries at that path. The
//! files are served; the *listing* came back empty.
//!
//! That is the exact failure `vfs-shim/src/hook.rs` warns about when an
//! enumeration entry point goes unhooked: it does not error, it "quietly
//! serves the real, near-empty directory, which reads exactly like a mod list
//! that is simply empty". A game shows it as missing mods; only a probe like
//! this shows it as a listing.
//!
//! # What it separates
//!
//! Exit 0 means the shim listed the directory over the ring. A failure says
//! which of three things happened, because they need different fixes:
//!
//! * the directory could not be opened at all — the *open* path, not
//!   enumeration;
//! * it opened and listed nothing — enumeration is not reaching the ring;
//! * it listed the wrong names — enumeration is reaching the ring and the
//!   entries are wrong.
//!
//! Usage: `dir-ring-client <dir-path> <expected-entry>`, with the same
//! `VFS_*` environment `shim-ring-client` takes.

#[cfg(windows)]
mod imp {
    use std::io::Write as _;
    use std::process::exit;

    use vfs_shim::{fuse_client, install, Engine};

    /// Give the shim's stats reporter — a thread on
    /// `VFS_SHIM_STATS_INTERVAL_MS` — a chance to write before the process
    /// goes away. Without this the probe's own speed hides the very rows that
    /// say *why* it failed.
    fn settle() {
        if vfs_env::present(vfs_env::SHIM_STATS_LOG) {
            std::thread::sleep(std::time::Duration::from_millis(2500));
        }
    }

    fn fail(msg: &str) -> ! {
        eprintln!("DIRCLIENT FAIL: {msg}");
        let _ = std::io::stderr().flush();
        settle();
        exit(1);
    }

    fn say(msg: &str) {
        println!("DIRCLIENT: {msg}");
        let _ = std::io::stdout().flush();
    }

    /// A valid, **empty** snapshot: the shim's local tree must contribute
    /// nothing, so every name in the listing came across the ring.
    fn empty_snapshot() -> Vec<u8> {
        let mut b = vfs_shared::SnapshotBuilder::new();
        let root = b.add_dir("", &[]);
        b.set_root(root);
        b.finish()
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        if args.len() != 3 {
            fail("usage: dir-ring-client <dir-path> <expected-entry>");
        }
        let dir = args[1].clone();
        let want = args[2].clone();

        let root = vfs_env::text(vfs_env::VIRTUAL_DIR)
            .unwrap_or_else(|| fail("VFS_VIRTUAL_DIR unset: the managed root has no default"));

        if let Err(e) = fuse_client::try_init_from_env() {
            fail(&format!("fuse init: {e:?}"));
        }
        let engine = Engine::new(&root, empty_snapshot())
            .unwrap_or_else(|e| fail(&format!("engine: {e:?}")));
        let _guard = install(engine).unwrap_or_else(|e| fail(&format!("install: {e:?}")));

        // Reported, not assumed. An enumeration entry point that was passed
        // over because this host's ntdll does not export it is the first
        // thing to suspect when a listing comes back empty.
        say(&format!("hooks installed, skipped: {:?}", vfs_shim::skipped_detours()));

        // An ordinary `read_dir`. Nothing here knows a ring exists — the same
        // call any game makes, through `FindFirstFileW` and whichever
        // `NtQueryDirectoryFile*` the host's kernelbase reaches for.
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| fail(&format!("read_dir {dir}: {e} — the directory could not be opened at all, which is the open path rather than enumeration")));

        let mut names: Vec<String> = Vec::new();
        for entry in entries {
            match entry {
                Ok(e) => names.push(e.file_name().to_string_lossy().into_owned()),
                Err(e) => say(&format!("entry error: {e}")),
            }
        }
        say(&format!("listed {} entr(y/ies): {:?}", names.len(), names));

        if names.is_empty() {
            fail(&format!(
                "{dir} listed EMPTY. The directory opened, so the ring is up and the open path \
                 works — enumeration is not reaching it. This is the failure that looks like an \
                 empty mod list."
            ));
        }
        if !names.iter().any(|n| n.eq_ignore_ascii_case(&want)) {
            fail(&format!("listed {names:?}, which does not contain {want:?}"));
        }
        say("OK");
        settle();
        exit(0);
    }
}

#[cfg(windows)]
fn main() {
    imp::main();
}

// `vfs-shim` is Windows-only (NT detours); this keeps a unix build green, the
// same shape its siblings use.
#[cfg(not(windows))]
fn main() {}
