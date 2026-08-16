//! The four DRM/identity filenames are no longer excepted: under a managed
//! root they are answered by the director like every other path, and the real
//! file underneath is never read from or written to (gate 5, Task 4).
//!
//! **What used to happen.** `try_fuse_create` matched four basenames —
//! `steam_appid.txt`, `SkyrimSELauncher.exe`, `steam_api{,64}.dll`,
//! `SkyrimSE.exe` — case-insensitively at any depth, and returned `None`
//! *before the ring was consulted*. The open then fell through to
//! `decision_for` -> `Engine::decide_open`, which either redirected it at a
//! real disk path or passed it straight through to the real filesystem under
//! the managed root. That was the last hole in the invariant this stage exists
//! to establish: *for any path under a managed root, every NT operation on it
//! is answered by the director.*
//!
//! **Why the fixture is shaped the way it is.** A test that only asserted "the
//! open returned success" would pass against the old code too — the excepted
//! open *did* succeed, by opening the wrong file. So every claim here is made
//! about bytes and about filesystem state:
//!
//! - The real files under the root exist and hold `HOST_*` bytes, which are
//!   different from the director's for every name. A read that comes back with
//!   host bytes is a read that reached the real file.
//! - The `Engine`'s snapshot maps all five names at their real on-disk paths,
//!   so the fall-through arm has somewhere to go. Without that the old code
//!   would merely have denied these opens, and this test would pass for a
//!   reason that has nothing to do with the exception being closed. This shape
//!   is also the live one: `skyrim-live` mounts the runtime root itself as a
//!   root-0 disk layer, so a composition really does know these names at their
//!   host paths.
//! - `steam_api.dll` is deliberately **not** served by the director while its
//!   real file exists on disk. That is the sealing half of the invariant: the
//!   director's not-found is the caller's not-found, even when a perfectly good
//!   file is sitting right there.
//! - The write half asserts the filesystem, not the status: the real
//!   `steam_appid.txt` must still hold its original bytes afterwards, and the
//!   payload must be in the director's own table.
//!
//! Its own binary: the detours, the `FuseClient`, the `Engine` and
//! `hookstats::enabled()` are process-global and resolve once.

mod fakedirector;

use fakedirector::{Fake, ReadStyle};
use vfs_shim::{install, outcome_count, Engine, OpenOutcome};

/// Bytes on the real filesystem under the managed root. One per name, so a
/// failure says *which* file was reached rather than only that one was.
const HOST_APPID: &[u8] = b"host: steam_appid.txt";
const HOST_LAUNCHER: &[u8] = b"host: SkyrimSELauncher.exe";
const HOST_API64: &[u8] = b"host: steam_api64.dll";
const HOST_API32: &[u8] = b"host: steam_api.dll";
const HOST_EXE: &[u8] = b"host: SkyrimSE.exe";

/// Bytes only the director has. They cannot be reached by any filesystem
/// route, so reading them is proof the ring answered.
const DIR_APPID: &[u8] = b"director: steam_appid.txt";
const DIR_LAUNCHER: &[u8] = b"director: SkyrimSELauncher.exe";
const DIR_API64: &[u8] = b"director: steam_api64.dll";
const DIR_EXE: &[u8] = b"director: SkyrimSE.exe";

/// What a write to `steam_appid.txt` must land at the director, and must not
/// land on disk.
const WRITTEN: &[u8] = b"written through the director";

/// `(on-disk name, folded vpath, host bytes)`. The folded vpath is what
/// `FuseClient::vpath_under_root` puts on the wire, and the case difference
/// between the two columns is deliberate: the exception matched
/// case-insensitively, so the fixture spells the names the way the game does.
const NAMES: &[(&str, &str, &[u8])] = &[
    ("steam_appid.txt", "steam_appid.txt", HOST_APPID),
    ("SkyrimSELauncher.exe", "skyrimselauncher.exe", HOST_LAUNCHER),
    ("steam_api64.dll", "steam_api64.dll", HOST_API64),
    ("steam_api.dll", "steam_api.dll", HOST_API32),
    ("SkyrimSE.exe", "skyrimse.exe", HOST_EXE),
];

#[test]
fn the_drm_names_are_answered_by_the_director_and_never_by_the_real_file() {
    let base = std::env::temp_dir().join(format!("vfs-drm-close-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(&root).unwrap();
    for (name, _, host) in NAMES {
        std::fs::write(root.join(name), host).unwrap();
    }

    // Counters on for the whole process; `hookstats::enabled()` resolves once,
    // so this must precede `install`. The reporter interval is pushed past the
    // test's lifetime because the assertions read `outcome_count` directly and
    // a reporter thread writing files inside a hooked process is only noise.
    std::env::set_var(vfs_env::SHIM_STATS_LOG, base.join("shim-stats.log"));
    std::env::set_var(vfs_env::SHIM_STATS_INTERVAL_MS, "3600000");

    // A snapshot that knows all five names at their *real* paths. This is the
    // fall-through arm's destination: with it, the old excepted open resolved
    // to `Decision::Redirect` and returned host bytes, which is the failure
    // this test has to be able to see. See the module doc.
    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let entries = NAMES
            .iter()
            .map(|(name, vpath, host)| InputEntry {
                vpath: (*vpath).into(),
                kind: EntryKind::File,
                source: root.join(name).to_string_lossy().as_ref().into(),
                size: host.len() as u64,
                mtime: 0,
            })
            .collect();
        let tree = build(vec![Layer { id: LayerId(0), entries }]).unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    // Four of the five are served. `steam_api.dll` is not, on purpose: its real
    // file exists, and the director's not-found has to be the caller's answer
    // anyway. Only `steam_appid.txt` is writable, which keeps the write half
    // narrow — everything else stays a read-only mount, as in a real session.
    let fake = fakedirector::install(
        &root,
        Fake::new()
            .with("steam_appid.txt", DIR_APPID.to_vec(), ReadStyle::Whole)
            .with("skyrimselauncher.exe", DIR_LAUNCHER.to_vec(), ReadStyle::Whole)
            .with("steam_api64.dll", DIR_API64.to_vec(), ReadStyle::Whole)
            .with("skyrimse.exe", DIR_EXE.to_vec(), ReadStyle::Whole)
            .writable_under("steam_appid.txt"),
        0,
    );

    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let hooks = install(engine).expect("install");

    // --- reads -------------------------------------------------------------
    let appid = std::fs::read(root.join("steam_appid.txt"));
    let launcher = std::fs::read(root.join("SkyrimSELauncher.exe"));
    let api64 = std::fs::read(root.join("steam_api64.dll"));
    let exe = std::fs::read(root.join("SkyrimSE.exe"));
    // The name the director does not serve, whose real file is right there.
    let api32 = std::fs::read(root.join("steam_api.dll"));

    // --- a write, which must reach the director and nothing else -----------
    let write_result = std::fs::write(root.join("steam_appid.txt"), WRITTEN);

    let drm_exceptions = outcome_count(OpenOutcome::FellThroughDrmException);
    let routed = outcome_count(OpenOutcome::Routed);

    // Everything below reads the real filesystem under the managed root, which
    // is only visible with the detours down.
    drop(hooks);

    assert_eq!(
        appid.as_deref().ok(),
        Some(DIR_APPID),
        "a read of steam_appid.txt under a managed root must come from the director"
    );
    assert_eq!(
        launcher.as_deref().ok(),
        Some(DIR_LAUNCHER),
        "a read of SkyrimSELauncher.exe under a managed root must come from the director"
    );
    assert_eq!(
        api64.as_deref().ok(),
        Some(DIR_API64),
        "a read of steam_api64.dll under a managed root must come from the director"
    );
    assert_eq!(
        exe.as_deref().ok(),
        Some(DIR_EXE),
        "a read of SkyrimSE.exe under a managed root must come from the director"
    );
    assert!(
        api32.is_err(),
        "steam_api.dll is not served by the director, so the open must fail — a real file \
         under a managed root that the provider graph never agreed to is unreachable, DRM \
         name or not. Got {:?}",
        api32.as_deref().map(String::from_utf8_lossy)
    );

    write_result.expect("a write to a path the director serves writably must succeed");

    // **The filesystem side, asserted before its weaker siblings.** A
    // fall-through write succeeds at the API and reports nothing wrong; the
    // only thing that catches it is the bytes under the root. This loop is
    // deliberately ordered ahead of the two director-side assertions below,
    // because those fire on the same mutation and would otherwise mask the one
    // claim a returned status cannot fake.
    for (name, _, host) in NAMES {
        let on_disk = std::fs::read(root.join(name)).unwrap();
        assert_eq!(
            on_disk.as_slice(),
            *host,
            "the real {name} under the managed root was modified — an open answered by the \
             director must not touch it"
        );
    }

    assert_eq!(
        fake.contents("steam_appid.txt").as_deref(),
        Some(WRITTEN),
        "the write must land in the director's own copy — that is where a later session and \
         every other process will look for it"
    );
    assert_eq!(
        fake.tally.writes("steam_appid.txt"),
        1,
        "the payload must have crossed the ring as an OP_WRITE; the right bytes at the \
         director with a zero here would mean something else put them there"
    );

    assert_eq!(
        drm_exceptions, 0,
        "`FellThroughDrmException` must stay at zero: the class is closed, and the counter \
         is kept precisely so a live session can prove it stayed closed"
    );
    assert!(
        routed >= 5,
        "the four served names plus the write should all be classified `Routed`; got {routed}"
    );
    for (name, vpath, _) in NAMES {
        if *vpath == "steam_api.dll" {
            continue; // never served, so never opened
        }
        assert!(
            fake.tally.opens(vpath) >= 1,
            "{name} never reached the director as an OP_OPEN"
        );
    }

    let _ = std::fs::remove_dir_all(&base);
}
