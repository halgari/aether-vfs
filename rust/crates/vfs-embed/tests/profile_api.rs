//! The profile API (`GetPrivateProfile*` / `WritePrivateProfile*`) against a
//! managed root's INI, read and write.

// Every test here injects a real Windows process, so on other hosts the helpers are unused.
#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

use std::collections::BTreeMap;
use std::sync::Arc;

use vfs_embed::{DiskProvider, LaunchOpts};

mod support;
use support::session::LiveSession;
use support::{artifacts::*, launch::*};

/// One tab-separated line of `vfs-fixture-prefs`' output: the operation, the
/// string the profile API produced (or `ok`/`fail` for a write), and the
/// `GetLastError` recorded straight after the call.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PrefsLine {
    op: String,
    value: String,
    lasterror: String,
}

fn parse_prefs_lines(text: &str) -> Vec<PrefsLine> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let mut f = l.split('\t');
            Some(PrefsLine {
                op: f.next()?.to_string(),
                value: f.next()?.to_string(),
                lasterror: f.next().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// The three strings this test has to tell apart. All three are things
/// `GetPrivateProfileStringW` can hand back *successfully*, which is why the
/// fixture's exit code cannot carry the answer and the harness matches on the
/// value.
const PREFS_VIRTUAL: &str = "VIRTUAL";

const PREFS_DISK: &str = "DISK";

const PREFS_DEFAULT: &str = "MISSING";

/// The Windows profile APIs (`GetPrivateProfileStringW` and family) must read
/// a file under a managed root **through the director**.
///
/// This is the investigation of 2026-08-14 promoted from a throwaway probe to
/// a permanent test. Skyrim loads `SkyrimPrefs.ini` this way, and a live
/// session showed 450 opens of it with zero reads. The cause was not the
/// profile APIs bypassing our hooks — they reach them and open through the
/// director correctly — but the call they make *next*: `NtLockFile`, which had
/// no detour, so the real kernel was handed a synthetic handle and answered
/// `STATUS_INVALID_HANDLE`. The API then abandoned the read and returned the
/// caller's own default. The game got no INI data at all.
///
/// **Three outcomes, not two.** A real file with different content sits on
/// disk at the same under-root path, so the value the fixture reports
/// separates:
///
/// - `VIRTUAL` — the director served it. The only pass.
/// - `DISK` — something read the real file behind the mount: a containment
///   escape, and a *different* bug from the one this test was written for.
/// - `MISSING` — the fixture's own default came back: nothing read anything.
///   This is what the unfixed tree produces, verified before the fix landed.
///
/// A test that only asserted "not MISSING" would pass on an escape, and one
/// that only compared against the director's bytes without a decoy on disk
/// could not tell a served read from a passthrough at all.
#[cfg(windows)]
#[test]
fn profile_api_reads_a_managed_root_ini_through_the_director() {
    let _guard = lock_launches();
    ensure_inject_artifacts();

    // CRLF: what an INI on Windows actually contains, and what the profile
    // API's own parser is fed in a live session.
    let virtual_ini = format!("[Display]\r\nsTest={PREFS_VIRTUAL}\r\niTest=42\r\n");
    let disk_ini = format!("[Display]\r\nsTest={PREFS_DISK}\r\niTest=7\r\n");

    let content_dir = vfs_testkit::tempdir().expect("tempdir");
    std::fs::write(content_dir.path().join("prefs.ini"), virtual_ini.as_bytes()).unwrap();

    let out_dir = vfs_testkit::tempdir().expect("out tempdir");
    let out_file = out_dir.path().join("prefs-out.tsv");
    let stats_dir = vfs_testkit::tempdir().expect("stats tempdir");
    let stats_log = stats_dir.path().join("shim-stats.log");

    let fixture = locate_artifact("vfs-fixture-prefs.exe");

    let mut session = LiveSession::create("prefs-read");

    session
        .add_source(0, "/", 0, Arc::new(DiskProvider::new(content_dir.path())))
        .expect("AddSource");

    // The decoy: a real file, physically present under the managed root, with
    // *different* content at the same path the fixture will ask for. Written
    // and read back from this never-injected harness process first, so
    // "the fixture did not see DISK" can never mean "DISK was never there".
    let root = session.root();
    let ini_path = root.join("prefs.ini");
    std::fs::write(&ini_path, disk_ini.as_bytes()).expect("write on-disk decoy");
    let decoy_readback = std::fs::read(&ini_path).expect("read decoy back");
    assert_eq!(
        decoy_readback,
        disk_ini.as_bytes(),
        "the on-disk decoy must really hold DISK content before the fixture runs, or the \
         VIRTUAL-vs-DISK discrimination below proves nothing"
    );

    let mut env = BTreeMap::new();
    env.insert(
        "VFS_FIXTURE_INI_PATH".to_string(),
        ini_path.to_string_lossy().into_owned(),
    );
    env.insert(
        "VFS_FIXTURE_INI_OUT".to_string(),
        out_file.to_string_lossy().into_owned(),
    );
    env.insert(
        "VFS_SHIM_STATS_LOG".to_string(),
        stats_log.to_string_lossy().into_owned(),
    );
    // Fast tick, same reason as the escape matrix's identical override: this
    // fixture's whole run is far under the reporter's 250ms default, and the
    // periodic reporter is the *only* writer of the report — nothing flushes
    // one at process exit (`vfs_shim::hookstats::banner` records why an exit
    // flush was built, measured, and removed). Paired with the fixture's own
    // end-of-run wait, which is what guarantees a tick lands after its last
    // call rather than merely somewhere during the run.
    env.insert("VFS_SHIM_STATS_INTERVAL_MS".to_string(), "5".to_string());

    let exit_code = launch_bounded(
        session.shared(),
        LaunchOpts {
            image: fixture.to_string_lossy().into_owned(),
            wait: true,
            env,
            ..Default::default()
        },
        "prefs fixture log",
    );
    assert_eq!(
        exit_code, 0,
        "the prefs fixture must exit 0 (it reports what it saw in its output file rather than \
         through its exit code; a nonzero exit means it could not run at all)"
    );

    let text = std::fs::read_to_string(&out_file).unwrap_or_default();
    let lines = parse_prefs_lines(&text);
    let reads: Vec<&PrefsLine> = lines.iter().filter(|l| l.op == "read").collect();
    assert_eq!(
        reads.len(),
        2,
        "expected two read lines from the fixture, got {lines:?} (raw: {text:?})"
    );

    for r in &reads {
        assert_ne!(
            r.value, PREFS_DEFAULT,
            "GetPrivateProfileStringW returned the caller's own default \
             ({PREFS_DEFAULT}, lasterror={}) for a file under a managed root: nothing served \
             the read at all. This is the NtLockFile signature — the open reaches the director, \
             the very next NT call on the synthetic handle is rejected by the real kernel, and \
             the profile API abandons the read. Lines: {lines:?}",
            r.lasterror
        );
        assert_ne!(
            r.value, PREFS_DISK,
            "the profile API read the *real* file behind the mount instead of the director's \
             content — a containment escape, not the lock bug. Lines: {lines:?}"
        );
        assert_eq!(
            r.value, PREFS_VIRTUAL,
            "the profile API returned neither the director's content, the real file's content, \
             nor its own default. Lines: {lines:?}"
        );
        assert_eq!(
            r.lasterror, "0",
            "a read that produced the right value must not also be reporting an error. \
             Lines: {lines:?}"
        );
    }

    // The primitive ladder: one assertion per NT call a synthetic handle has
    // to survive. The profile read above stops at the first failure, so
    // without these `NtUnlockFile` and `NtFlushBuffersFile` would have no
    // coverage at all — the read never gets far enough to reach them.
    for op in [
        "open",
        "getfiletype",
        "setfilepointer",
        "lockfile",
        "unlockfile",
        "flushbuffers",
        // `LockFileEx` with an OVERLAPPED event — the asynchronous-shaped
        // lock. A plain `LockFile` passes a null event, so only this one
        // exercises the completion the shim has to signal.
        "lockfileex",
    ] {
        let line = lines
            .iter()
            .find(|l| l.op == op)
            .unwrap_or_else(|| panic!("no `{op}` line from the fixture: {lines:?}"));
        assert_eq!(
            line.value, "ok",
            "`{op}` failed (lasterror={}) on a handle to a file under a managed root. A \
             synthetic handle is not a kernel object, so every NT call without a detour is \
             rejected outright — and the caller's next move is usually to abandon the file, \
             not to retry. Lines: {lines:?}",
            line.lasterror
        );
    }

    // The other half of the same question: which handles must **not** be
    // answered. A synthetic handle is marked by a single tag bit, and
    // `INVALID_HANDLE_VALUE` (-1) has every bit set — so a hook that decides on
    // the tag alone reports a lock taken on a handle nobody owns, and the same
    // for one closed a moment earlier. Both are worse than an error: they
    // convert a caller's failed open into a lock it believes it holds.
    for op in [
        "lockfile-closed",
        "unlockfile-closed",
        "flushbuffers-closed",
        "lockfile-invalid",
        "unlockfile-invalid",
        "flushbuffers-invalid",
    ] {
        let line = lines
            .iter()
            .find(|l| l.op == op)
            .unwrap_or_else(|| panic!("no `{op}` line from the fixture: {lines:?}"));
        assert_eq!(
            line.value, "fail",
            "`{op}` succeeded. The handle is either closed or was never valid, so the only \
             correct answer is failure — a synthetic-handle branch that keys off the tag bit \
             without resolving the handle grants locks on handles that do not exist. \
             Lines: {lines:?}"
        );
    }

    // The read must not have disturbed the real file either: the profile API
    // opens INIs for read/write in places, and a "successful" read that
    // rewrote the decoy would be a different failure wearing this test's pass.
    assert_eq!(
        std::fs::read(&ini_path).expect("re-read decoy"),
        disk_ini.as_bytes(),
        "the real on-disk file under the managed root was modified by a read-only profile-API call"
    );

    // ── the instrument, not only the bytes ──
    let report = std::fs::read_to_string(&stats_log).unwrap_or_default();
    assert!(
        !report.is_empty(),
        "the shim wrote no stats report at {stats_log:?}, so every assertion below would pass \
         or fail for the wrong reason"
    );

    // Asserted **before** anything is read out of the report: every report
    // this shim writes is a point-in-time snapshot (there is no exit flush —
    // `vfs_shim::hookstats::banner` records why one was tried and removed), so
    // a reader has to know which point. Measured during this task: a report
    // written before the fixture's own end-of-run wait genuinely lacks the
    // `NtLockFile` row, and every row assertion below would then fail for a
    // reason that has nothing to do with the hooks. Checking the banner first
    // is what keeps those failures readable.
    assert!(
        report.starts_with("SNAPSHOT at t+"),
        "the report does not open with its snapshot banner, so nothing below can tell \"this \
         hook never ran\" from \"this hook ran after the last tick\". First line: {:?}",
        report.lines().next().unwrap_or_default()
    );

    // Each hook's own row. `render` omits any hook with zero calls, so a
    // present row means the hook ran *and* was counted — which is the half
    // that was missing for NtSetInformationFile and NtQueryVolumeInformationFile
    // (neither had a `hookstats::Timed` guard, unlike every other hook, so
    // calls to them were invisible in this table).
    for hook in [
        "NtLockFile",
        "NtUnlockFile",
        "NtFlushBuffersFile",
        "NtSetInformationFile",
        "NtQueryVolumeInformationFile",
    ] {
        assert!(
            report.contains(hook),
            "the shim's hook table has no `{hook}` row, though the fixture demonstrably made \
             that call (its own ladder line above says it succeeded). An uncounted hook reads \
             as a hook that never ran. Report:\n{report}"
        );
    }

    // The deliberate semantic gap, made visible: locks granted on synthetic
    // handles are locks nobody holds, and the report says so by path.
    //
    // Sliced to the section rather than searched across the whole report:
    // `prefs.ini` appears in the trace and outcome sections of the same file
    // for entirely unrelated reasons, so a bare `report.contains` here would
    // assert nothing about the lock section at all.
    const SYNTH_LOCK_HEADER: &str = "synthetic byte-range locks";
    let idx = report.find(SYNTH_LOCK_HEADER).unwrap_or_else(|| {
        panic!(
            "no synthetic-lock section in the report, though the fixture locked a file under a \
             managed root. That section is the only record that a lock was granted without \
             being taken. Report:\n{report}"
        )
    });
    let section = report[idx..]
        .lines()
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    assert!(
        section.contains("lock-exclusive") || section.contains("lock-shared"),
        "the synthetic-lock section names no lock operation. Section:\n{section}"
    );
    assert!(
        section.contains("prefs.ini"),
        "the synthetic-lock section does not name the file that was locked, so it cannot answer \
         \"is anyone else locking this\" — the one question it exists for. Section:\n{section}"
    );

    // Completion shape. The shim signals a caller's event but never runs a
    // caller's APC, so a caller that waits on one hangs — and the async
    // section is the only place that shape is visible. `lock_hook` has to
    // classify its calls the way `read_hook`/`write_hook` classify theirs or
    // an APC-supplied lock is neither delivered nor counted.
    //
    // Attributable: every other synthetic-handle operation this fixture makes
    // is fully synchronous (null event), so the `with event` count can only
    // have come from the `LockFileEx` above.
    let with_event = report
        .lines()
        .find_map(|l| {
            let rest = l.trim().strip_prefix("reads:")?;
            let (_, after_apc) = rest.split_once('/')?;
            after_apc
                .trim()
                .strip_suffix(" with event")
                .or_else(|| after_apc.split_once(" with event").map(|(n, _)| n))
        })
        .and_then(|n| n.trim().parse::<u64>().ok())
        .unwrap_or_else(|| {
            panic!("could not read the async section's completion counts. Report:\n{report}")
        });
    assert!(
        with_event >= 1,
        "the async section counts no event-completed operations, though the fixture's \
         `LockFileEx` supplied one and reported success. An unclassified lock is one the \
         report cannot show waiting on a completion we never deliver. Report:\n{report}"
    );
}

/// The write half of the same mechanism: `WritePrivateProfileStringW` must
/// land its bytes **at the director**, not nowhere.
///
/// The live session that started the investigation showed 300 successful write
/// opens of `SkyrimPrefs.ini` and zero write operations. That is
/// `WritePrivateProfileStringW` hitting the same unhooked `NtLockFile` one
/// operation earlier than the read path does: the create succeeds, the lock is
/// rejected, and the API reports failure to its caller having written nothing.
///
/// The decisive assertion is not that the value reads back — a shim-local
/// overlay or the real file behind the mount would satisfy that too — but that
/// the **provider's own backing file** on disk holds the new value while the
/// decoy under the session root is untouched.
#[cfg(windows)]
#[test]
fn profile_api_writes_a_managed_root_ini_through_the_director() {
    let _guard = lock_launches();
    ensure_inject_artifacts();

    const PREFS_WRITTEN: &str = "WRITTEN";
    let virtual_ini = format!("[Display]\r\nsTest={PREFS_VIRTUAL}\r\niTest=42\r\n");
    let disk_ini = format!("[Display]\r\nsTest={PREFS_DISK}\r\niTest=7\r\n");

    let content_dir = vfs_testkit::tempdir().expect("tempdir");
    let backing = content_dir.path().join("prefs.ini");
    std::fs::write(&backing, virtual_ini.as_bytes()).unwrap();

    let out_dir = vfs_testkit::tempdir().expect("out tempdir");
    let out_file = out_dir.path().join("prefs-write-out.tsv");

    let fixture = locate_artifact("vfs-fixture-prefs.exe");

    let session = LiveSession::create("prefs-write");

    session
        .set_write_layer(0, Arc::new(DiskProvider::new(content_dir.path())))
        .expect("AddSource");

    let root = session.root();
    let ini_path = root.join("prefs.ini");
    std::fs::write(&ini_path, disk_ini.as_bytes()).expect("write on-disk decoy");

    let mut env = BTreeMap::new();
    env.insert(
        "VFS_FIXTURE_INI_PATH".to_string(),
        ini_path.to_string_lossy().into_owned(),
    );
    env.insert(
        "VFS_FIXTURE_INI_OUT".to_string(),
        out_file.to_string_lossy().into_owned(),
    );
    env.insert(
        "VFS_FIXTURE_INI_WRITE".to_string(),
        PREFS_WRITTEN.to_string(),
    );

    let exit_code = launch_bounded(
        session.shared(),
        LaunchOpts {
            image: fixture.to_string_lossy().into_owned(),
            wait: true,
            env,
            ..Default::default()
        },
        "prefs fixture log",
    );
    assert_eq!(exit_code, 0, "the prefs fixture must exit 0");

    let text = std::fs::read_to_string(&out_file).unwrap_or_default();
    let lines = parse_prefs_lines(&text);
    let write = lines
        .iter()
        .find(|l| l.op == "write")
        .unwrap_or_else(|| panic!("no `write` line from the fixture: {lines:?} (raw: {text:?})"));
    assert_eq!(
        write.value, "ok",
        "WritePrivateProfileStringW reported failure (lasterror={}) for a file under a managed \
         root. lasterror 6 (ERROR_INVALID_HANDLE) is the unhooked-NT-call signature: the create \
         reached the director and the very next call on the synthetic handle did not. \
         Lines: {lines:?}",
        write.lasterror
    );

    for r in lines.iter().filter(|l| l.op == "read") {
        assert_eq!(
            r.value, PREFS_WRITTEN,
            "the value written through the profile API did not read back through it. \
             Lines: {lines:?}"
        );
    }

    // The decisive pair. The provider's own file must hold the new value —
    // that is what "the write crossed the ring" means, and neither a
    // shim-local overlay nor the real file behind the mount can produce it.
    let backing_after = std::fs::read_to_string(&backing).expect("read provider backing file");
    assert!(
        backing_after.contains(PREFS_WRITTEN),
        "the director's backing file does not contain the written value, so the write landed \
         somewhere else (or nowhere). Backing file now: {backing_after:?}"
    );
    // …and the real file physically under the session root must be untouched.
    assert_eq!(
        std::fs::read(&ini_path).expect("re-read decoy"),
        disk_ini.as_bytes(),
        "the write escaped to the real file under the managed root instead of going to the \
         director"
    );
}
