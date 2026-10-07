//! The escape matrix: every path spelling of `vfs-fixture-escape` against a
//! managed root, read and write, plus the metadata-query seal.

// Every test here injects a real Windows process, so on other hosts the helpers are unused.
#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::TcpListener;
use tonic::transport::Server;
use vfs_control::pb::director_server::DirectorServer;
use vfs_directord::{connect, DirectorService, SessionRegistry};

mod support;
use support::{escape::*, launch::*, artifacts::*};

/// The full, fixed vector-id order `vfs-fixture-escape` emits — used to
/// assert every expected line actually showed up (a vector silently
/// missing from the output would otherwise read as "nothing to check"
/// rather than the fixture-contract violation it would be).
/// `3b`/`3c`/`3d` are the object-manager spellings that reach the target
/// through its drive letter (`\??\GLOBALROOT\GLOBAL??\C:\...` and two
/// siblings) rather than through a device name, which is all vector `3` ever
/// built. Every assertion in this file applies to them through the same
/// catch-all expectation tables as the rest — they are ordinary vectors, not
/// caveats — and each was verified to fail here when the canonicaliser fix
/// they cover is reverted.
const ALL_VECTOR_IDS: &[&str] = &[
    "1", "2", "3", "3b", "3c", "3d", "4", "5", "5b", "6", "7", "8", "9", "10a", "10b", "10c", "11",
    "12a", "12b", "12c", "13", "14",
];

/// This gate's own scope note (`docs/superpowers/plans/...`): vectors 13
/// and 14 are reported, not closed, here — 13 needs gate 3's timing fix, 14
/// may not be a shim fix at all. Neither gets a strict outcome assertion in
/// either canary; both still have their line printed and preserved in the
/// matrix, with the fixture's own "reported, not closed" note carried
/// through, so a reader can never mistake a blank for a pass.
fn is_reported_not_closed(vector: &str) -> bool {
    matches!(vector, "13" | "14")
}

/// The positive canary's expected outcome per vector, or `None` for a
/// vector this test does not assert an exact outcome for (the two
/// reported-not-closed vectors, and `5b`'s documented caveat — see its own
/// doc comment in `vfs-fixture-escape`).
///
/// Every other buildable vector must open the real bytes: this is the half
/// of the matrix the brief calls "fully assertable now... the cheap way to
/// pass a containment test is to break all access, and the positive canary
/// is what forbids that."
fn positive_expectation(vector: &str) -> Option<&'static str> {
    if vector == "5b" || is_reported_not_closed(vector) {
        return None;
    }
    // Stage 2b task 5 flip, covering vectors 1, 3, 4, 7 and 9 together: all
    // five are back to the catch-all `Some("opened")` below, which is where
    // they started before Gate 3 Task 5 moved them out.
    //
    // Why they were `not-found` in between: those five are recognised as
    // under-root *only* by `RootMap::compute_under_root`'s canonicalisation
    // (`vfs-redirect`'s device/volume-GUID/GLOBALROOT/UNC-admin-share/
    // junction-alias tables), and `FuseClient::vpath_under_root` — the
    // shim-side router deciding whether an open reaches the director at all —
    // used to be a *second*, plain string-prefix predicate with none of those
    // tables. So `try_fuse_create` gave up for all five spellings and fell
    // through to `decision_for`/`RootMap`, which in a live session resolves
    // against the shim's embedded empty-tree snapshot and answered
    // `SnapResolution::NotFound` no matter what the director actually had.
    // Gate 3 Task 5 sealed that `NotFound` (correctly), which is what turned
    // these five from `opened`-via-real-disk into `not-found`.
    //
    // What changed: task 5 deleted the second predicate.
    // `FuseClient::vpath_under_root` *is* a `RootMap` now, so these five
    // spellings route to the director like any ordinary path, and the
    // director genuinely has the positive canary's content — so they open
    // through the director rather than by reading the byte-identical real
    // file on `session.root`, which is the outcome gate 3 was reaching for.
    //
    // The `negative_expectation` side is unchanged and still `not-found` for
    // all five: routing them to the director does not make a file no provider
    // serves appear. Together those two are the containment claim — reachable
    // when a provider has it, sealed when none does — for every spelling this
    // fixture can build, not merely for the ordinary one.
    match vector {
        // A hardlink names the SAME bytes under a brand-new file name the
        // content-addressed provider has never heard of. FUSE-routing (the
        // shim's pre-existing, gate-2-independent `vpath_under_root`
        // matcher, not this gate's canonicaliser) recognises the ordinary,
        // unmangled path and asks the director for that name first; the
        // director correctly answers "no such name" (`ST_NOT_FOUND`), and
        // with disk-fallthrough at its secure default (off,
        // `VFS_ALLOW_DISK_FALLTHROUGH` unset), that answer is sealed rather
        // than falling through to the real, hardlinked bytes still sitting
        // on disk. Verified by reproduction, not assumed: this is an
        // inherent property of naming the same bytes under a name the
        // content model has never seen — orthogonal to gate 2's
        // canonicaliser, which is never even consulted for this vector
        // whenever FUSE-routing claims the path first.
        "8" => Some("not-found"),
        // Read-only, `OPEN_EXISTING`, against a stream this fixture never
        // pre-creates (see the fixture's own vector-11 doc comment) —
        // legitimately absent, standalone or under a session.
        "11" => Some("not-found"),
        _ => Some("opened"),
    }
}

/// The negative canary's expected outcome for a **read** open, or `None` for
/// a vector this check does not apply to strictly — the same two documented
/// exceptions `positive_expectation` already carries:
///
/// - `"5b"`: not an alternate classification of the negative canary at all.
///   `OBJECT_ATTRIBUTES.RootDirectory` pointing at an anonymous pipe fails
///   the construction itself at the NT level (`error:ntstatus:...`,
///   independent of which target is named), so there is no "reachable vs.
///   not-found" question to assert here regardless of target.
/// - `"13"`/`"14"` (`is_reported_not_closed`): per this gate's own scope
///   note, neither vector gets a strict outcome assertion in either canary.
///   `"14"` spawns a child process, on the long-standing assumption that the
///   child runs with **no shim injected** and so reads the real, physical
///   negative-canary bytes directly. **Measured otherwise in gate 4 task 8**:
///   the shim hooks `CreateProcessInternalW` and injects into children
///   (`vfs-shim/src/hook/process.rs`), so the child is injected and this line actually
///   reports `error:cmd-exit:1` — the real bytes were *not* reachable. Still
///   unasserted here; the shim's `child_inject_fails_closed` test covers the
///   case where the injection fails (the child is killed, never released
///   unhooked). See `rust/docs/escape-matrix.md`, "Gate 4, Task 8".
///
/// Every other buildable vector must now come back `not-found`: Gate 3 Task
/// 5 stopped `RootMap::decide` passing `NotFound`/`Dir` through, and the
/// director itself answers "no such name" for any spelling that reaches it
/// with disk-fallthrough off — so a real, on-disk file under root that no
/// provider serves is unreachable by a read, for every spelling this fixture
/// can build, not merely classified into a counted bucket while still
/// secretly readable. This is a stronger claim than `classification_marker`
/// below checks, and the two are asserted separately in the test body — see
/// this function's own call site for why classified-but-reachable is exactly
/// the failure mode that made "classification, not containment" the matrix's
/// standing caveat before this task.
fn negative_expectation(vector: &str) -> Option<&'static str> {
    if vector == "5b" || is_reported_not_closed(vector) {
        return None;
    }
    Some("not-found")
}

/// The substring this test searches for in the shim's classified-paths set
/// (see `support::classified_paths`) to decide whether a given vector's
/// attempt was classified under-root, or `None` for a vector this check
/// does not apply to.
///
/// `"14"` is excluded unconditionally, though **not for the reason this
/// comment used to give**. It said the child runs with no shim injected at
/// all, so its open happens in a process whose hook stats this test can never
/// see. The shim in fact detours `CreateProcessInternalW` and injects into
/// children (`vfs-shim/src/hook/process.rs`; measured in gate 4 task 8 — see
/// `rust/docs/escape-matrix.md`), so the child is hooked and, inheriting
/// `VFS_SHIM_STATS_LOG`, may even report into this same file.
///
/// The exclusion stands anyway, and is now the stronger claim rather than the
/// weaker one: whatever appears is a *different process's* classification of
/// its own open, not this vector's, and whether it appears at all depends on
/// when that child runs relative to this test's read of the file. Presence and
/// absence are both scheduling outcomes here, so neither is evidence about gate 2.
///
/// `"5b"` is excluded too, but for the opposite reason: it *is* an
/// in-process, hooked open, but one whose `OBJECT_ATTRIBUTES.RootDirectory`
/// (an anonymous pipe) `GetFinalPathNameByHandleW` cannot resolve, so
/// `path_of_tracked` never decodes a path for it at all — the open is real
/// but genuinely un-decodable, landing in the shim's separate "undecodable"
/// counter, never in "under-root open outcomes". That is Task 4's
/// documented, accepted edge (falls back to the pre-existing passthrough),
/// not a gate-2 classification miss — see this vector's own note in the
/// matrix.
///
/// `"7"` (junction) and `"9"` (UNC admin share) **used to be excluded here
/// too**, for a third, more serious reason: verified by isolated
/// reproduction, they genuinely did not classify — both resolve to the real
/// bytes via a syntactically unrelated path (a different directory tree for
/// the junction; a `UNC\localhost\C$\...` form for the admin share) that
/// contains no `~`, so `RootMap::compute_under_root` never reached its
/// OS-consult branch (`expand_short_name`), the only place a syntactically
/// unrelated path like these could ever be recognised.
///
/// Both are now closed by resolving each into a `VolumeMap` alias **once at
/// session start**, the same pattern the device/volume-GUID table already
/// uses, rather than widening the per-open OS-consult gate: `vfs-redirect`'s
/// `resolve_volume_map` now also (a) registers `\??\UNC\localhost\<drive>$`
/// as an alias for `<drive>:` for every mounted drive, and (b) walks the
/// managed root's own ancestor chain, one non-recursive directory listing
/// per level, registering any *sibling* reparse point whose resolved target
/// lands inside the root. See `vfs-redirect/src/volumes.rs`'s
/// `junction_aliases` and `admin_share_nt_key` doc comments for the full
/// mechanism, the scope this task deliberately chose (and rejected), and
/// `rust/docs/escape-matrix.md` for the verified before/after.
fn classification_marker(vector: &str, basename: &str) -> Option<String> {
    match vector {
        "14" | "5b" => None,
        // The hardlink itself, not the original canary name.
        "8" => Some("vfs-escape-hardlink".to_string()),
        _ => Some(basename.to_ascii_lowercase()),
    }
}

/// Task 6: the canary matrix. Runs `vfs-fixture-escape` under a real,
/// composed session — daemon, director, injected shim, the works — against
/// two targets, and checks the two halves the gate's scope note draws:
///
/// - **Positive canary** (`escape-positive-canary.esp`, mirrored
///   byte-for-byte into both the `DiskProvider`'s backing directory and the
///   physical, on-disk managed root): every buildable spelling must open
///   it, with the *exact same bytes* — checked by the fixture itself
///   (`vfs-fixture-escape` now reads every successful open back and
///   compares against a baseline read of the literal path, failing closed
///   to `error:content-mismatch:...` on any difference; see that crate's
///   module doc). This is the half that forbids "pass by breaking
///   everything".
/// - **Negative canary** (`escape-negative-canary.bin`, a real file
///   physically on the managed root that the `DiskProvider` never serves):
///   two properties are now asserted, not one.
///   - **Classified** — every buildable spelling still appears in a counted
///     outcome bucket in the shim's own hook-stats report, checked in
///     isolation (`VFS_ESCAPE_ONLY_VECTOR`) to rule out riding on another
///     vector's entry — this is the gate-2-era property, unchanged.
///   - **Unreachable, Gate 3 Task 6's own addition**: every buildable
///     spelling's **read** open must come back `not-found`. Before this
///     gate, "classified" and "reachable" could both be true for the same
///     vector at once (see `rust/docs/escape-matrix.md`'s "second, structural
///     finding") — classification alone was never proof of containment.
///     This is the assertion that closes that gap: a vector that is merely
///     classified while still opening the real bytes now fails this test.
///     Scoped to reads only, because when this was written a **write** open
///     still reached the negative canary through the shim-local
///     engine's `cow_seed` last-resort branch (since deleted). Gate 4's Task 5 deleted that branch, and
///     `escape_matrix_write_access_positive_and_negative_canary` below now
///     asserts the write half — so the scope note describes this test's
///     coverage, not a remaining hole. `5b` (undecodable handle-relative
///     open) and the two reported-not-closed vectors (`13`, `14`) are exempt
///     from this assertion for the same documented reasons the positive
///     canary's own `positive_expectation` already exempts them — see
///     `negative_expectation`'s doc comment.
///
/// A stack-overflow crash was found and fixed while building this test (see
/// `vfs_redirect`'s `OS_CONSULT_DEPTH` guard) — vector 1 (8.3 short name)
/// recursed without bound the first time this matrix was run against a
/// *served* target, because `RootMap::compute_under_root`'s OS-consult
/// branch made its own hooked `CreateFileW` call with no re-entrancy guard.
/// See `task-6-report.md` for the full account.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn escape_matrix_positive_and_negative_canary() {
    let _guard = LAUNCH_LOCK.lock().await;
    ensure_inject_artifacts();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let registry = SessionRegistry::new();
    let svc = DirectorService::new(registry);
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // The DiskProvider's backing store — deliberately NOT session.root, so
    // the negative canary (written only to session.root below) is a real
    // file under the managed root that this provider genuinely does not
    // have, rather than something this test would have to fake.
    let content_dir = tempfile::tempdir().expect("tempdir");
    let stats_dir = tempfile::tempdir().expect("stats tempdir");
    let stats_log = stats_dir.path().join("shim-stats.log");
    let out_dir = tempfile::tempdir().expect("out tempdir");
    let out_file = out_dir.path().join("escape-out.tsv");

    let fixture = locate_artifact("vfs-fixture-escape.exe");
    let mut client = connect(&format!("{addr}")).await.expect("connect");

    let session = client
        .create_session(vfs_control::pb::CreateSessionReq {
            name: "escape-matrix".into(),
        })
        .await
        .expect("CreateSession")
        .into_inner();
    assert!(!session.id.is_empty());
    assert!(!session.root.is_empty());

    use vfs_control::pb::{source_spec, AddSourceReq, DiskSource, SourceSpec as PbSource};

    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(PbSource {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: content_dir.path().to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 0,
            root: 0,
            write_layer: false,
            cache_key: String::new(),
        })
        .await
        .expect("AddSource");

    let root = PathBuf::from(&session.root);
    let sub = PathBuf::from("Games").join("Skyrim").join("Data");
    std::fs::create_dir_all(root.join(&sub)).expect("mkdir under session root");
    std::fs::create_dir_all(content_dir.path().join(&sub)).expect("mkdir under content dir");

    // Positive canary: identical bytes physically on session.root AND in
    // the DiskProvider's backing dir. Whichever mechanism actually serves a
    // given spelling — FUSE-routed (the director, reading content_dir) or
    // real-disk passthrough (reading session.root directly) — the bytes are
    // the same either way, so "opened" is a meaningful byte-identity
    // signal regardless of which path served it.
    const POSITIVE_BASENAME: &str = "escape-positive-canary.esp";
    const POSITIVE_BYTES: &[u8] = b"the-positive-canary-bytes";
    let pos_rel = sub.join(POSITIVE_BASENAME);
    std::fs::write(root.join(&pos_rel), POSITIVE_BYTES).expect("write positive canary (root)");
    std::fs::write(content_dir.path().join(&pos_rel), POSITIVE_BYTES)
        .expect("write positive canary (content_dir)");

    // Negative canary: real bytes ONLY on session.root — a file under the
    // managed root that the DiskProvider's backing dir never has.
    const NEGATIVE_BASENAME: &str = "escape-negative-canary.bin";
    let neg_rel = sub.join(NEGATIVE_BASENAME);
    std::fs::write(root.join(&neg_rel), b"the-negative-canary-bytes")
        .expect("write negative canary");

    // Vector 7's junction, created here — by this test harness's own,
    // never-injected process — rather than by the fixture at runtime. See
    // `VFS_ESCAPE_VECTOR7_LINK_DIR`'s doc comment in `vfs-env` for why: the
    // fixture spawning `mklink /J` itself would be real, hooked file
    // activity inside the injected process, racing `vfs-redirect`'s
    // once-per-session volume/junction resolution (found by reproduction —
    // an isolated vector-7 run consistently failed to classify until this
    // moved out of the fixture). Points at the shared `Data` directory both
    // canaries live in, so one junction covers both.
    let vector7_link = std::env::temp_dir().join(format!("vfs-escape-junction-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir(&vector7_link);
    let vector7_link_ready = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &vector7_link.to_string_lossy(),
            &root.join(&sub).to_string_lossy(),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let vector7_link_dir = vector7_link_ready.then(|| vector7_link.to_string_lossy().into_owned());
    let ctx = EscapeFixtureCtx {
        session_id: &session.id,
        fixture: &fixture,
        stats_log: &stats_log,
        vector7_link_dir: vector7_link_dir.as_deref(),
        write_access: false,
    };

    // ---------------------------------------------------------------
    // Positive canary: every buildable spelling opens it, byte-identical.
    // ---------------------------------------------------------------
    let (pos_exit, pos_lines, _pos_classified, _pos_truncated) =
        run_escape_fixture(&mut client, &ctx, &root.join(&pos_rel), &out_file, None).await;
    if std::env::var("VFS_TEST_MATRIX_DUMP").is_ok() {
        eprintln!("=== POSITIVE lines ===");
        for l in &pos_lines {
            eprintln!("{}\t{}\t{}\t{}", l.vector, l.spelling, l.outcome, l.note);
        }
    }
    assert_eq!(
        pos_exit, 0,
        "vfs-fixture-escape must exit 0 against the positive canary — a nonzero/crash exit \
         (e.g. STATUS_STACK_OVERFLOW, -1073741571) means a vector took the process down before \
         the rest of the matrix could even be attempted, which is worse than any single \
         vector's own outcome. Lines captured before the crash: {pos_lines:?}"
    );
    for id in ALL_VECTOR_IDS {
        assert!(
            pos_lines.iter().any(|l| &l.vector == id),
            "positive canary: vector {id} produced no line at all in {out_file:?} — a missing \
             line must never be readable as a pass"
        );
    }
    for line in &pos_lines {
        let Some(want) = positive_expectation(&line.vector) else { continue };
        if line.outcome.starts_with("unbuildable:") {
            // A first-class, environment-dependent outcome — recorded in
            // the matrix, not a failure of this assertion.
            continue;
        }
        assert_eq!(
            line.outcome, want,
            "positive canary vector {}: expected `{want}`, got `{}` (spelling: {:?}, note: {:?})",
            line.vector, line.outcome, line.spelling, line.note
        );
    }

    // ---------------------------------------------------------------
    // Negative canary: every buildable spelling is classified under-root
    // (appears in the shim's own counted outcome buckets), never merely
    // "reachable" and never invisible as outside-root. See
    // `rust/docs/escape-matrix.md` for what this half does and does not
    // establish.
    // ---------------------------------------------------------------
    let (neg_exit, neg_lines, neg_classified, neg_truncated) =
        run_escape_fixture(&mut client, &ctx, &root.join(&neg_rel), &out_file, None).await;
    if std::env::var("VFS_TEST_MATRIX_DUMP").is_ok() {
        eprintln!("=== NEGATIVE lines ===");
        for l in &neg_lines {
            eprintln!("{}\t{}\t{}\t{}", l.vector, l.spelling, l.outcome, l.note);
        }
        eprintln!("=== NEGATIVE classified set (truncated={neg_truncated}) ===");
        for p in &neg_classified {
            eprintln!("{p}");
        }
    }
    assert_eq!(
        neg_exit, 0,
        "vfs-fixture-escape must exit 0 against the negative canary too. Lines captured: {neg_lines:?}"
    );
    for id in ALL_VECTOR_IDS {
        assert!(
            neg_lines.iter().any(|l| &l.vector == id),
            "negative canary: vector {id} produced no line at all in {out_file:?}"
        );
    }
    assert!(
        !neg_truncated,
        "the shim report's per-outcome path list was truncated (more distinct paths in one \
         outcome bucket than `hookstats::OUTCOME_PATHS_SHOWN`) — this test's per-vector \
         classification search below cannot be \
         trusted against a truncated list, so this must never happen for a run this small. \
         Report: {stats_log:?}"
    );
    // ---------------------------------------------------------------
    // Gate 3, Task 6: the negative canary is now unreachable, not merely
    // classified. Each `EscapeLine` is already tagged with its own vector,
    // so — unlike the classification check below — this needs no isolated
    // re-run to avoid riding on another vector's effect: `line.outcome` is
    // this vector's own attempt's own result, from this combined run.
    //
    // This is the assertion this task adds, and it is strictly stronger than
    // "classified": before Gate 3 Task 5, a spelling could be classified
    // (land in a counted bucket) while still opening the real bytes on
    // `session.root` (see "A second, structural finding" in
    // `rust/docs/escape-matrix.md` — vectors 1/3/4/7/9 were exactly this).
    // Scoped to reads only, per the brief. When that scope was set, a write
    // open still reached this same file through the shim-local
    // engine's `cow_seed` last-resort branch (since deleted); gate 4's Task 5 deleted it, and the write half is
    // asserted by `escape_matrix_write_access_positive_and_negative_canary`.
    for line in &neg_lines {
        let Some(want) = negative_expectation(&line.vector) else { continue };
        if line.outcome.starts_with("unbuildable:") {
            continue; // Never attempted at the OS level; nothing to seal.
        }
        assert_eq!(
            line.outcome, want,
            "negative canary vector {}: expected `{want}` — a real file on session.root that no \
             provider serves must be unreachable by a read, for every buildable spelling, not \
             merely classified while still readable — got `{}` (spelling: {:?}, note: {:?})",
            line.vector, line.outcome, line.spelling, line.note
        );
    }
    // The combined run above shares one shim-stats report across all
    // twenty-two attempts, and the report's classified-paths set is not keyed
    // by vector — several *different* spellings legitimately canonicalise
    // to the identical recorded path (that collapsing is the whole point of
    // the canonicaliser), so "some entry contains this vector's marker" in
    // the combined set does not prove *this* vector's own attempt was the
    // one that produced it. A vector whose own attempt was silently
    // unclassified (outside-root, invisible) would still pass that check
    // for free, riding on an unrelated vector's classified entry that
    // happens to share the same filename substring — exactly the "silently
    // probed nothing and reported closed" failure this project has hit
    // before. Re-run each buildable vector *alone* (`VFS_ESCAPE_ONLY_VECTOR`
    // — see `vfs-fixture-escape`'s module doc), so its isolated run's
    // classified set can only ever contain its own attempt's effect, plus
    // the handful of incidental opens (parent-directory probes, etc.) every
    // launch makes regardless of which vector is selected.
    for line in &neg_lines {
        if line.outcome.starts_with("unbuildable:") {
            continue; // Never attempted at the OS level; nothing to classify.
        }
        let Some(marker) = classification_marker(&line.vector, NEGATIVE_BASENAME) else {
            continue; // `5b` / `14` — see `classification_marker`'s doc comment.
        };
        let (iso_exit, iso_lines, iso_classified, iso_truncated) = run_escape_fixture(
            &mut client,
            &ctx,
            &root.join(&neg_rel),
            &out_file,
            Some(line.vector.as_str()),
        )
        .await;
        assert_eq!(
            iso_exit, 0,
            "negative canary, isolated run for vector {}: must exit 0. Lines: {iso_lines:?}",
            line.vector
        );
        assert!(!iso_truncated, "isolated run for vector {} truncated its path list", line.vector);
        if std::env::var("VFS_TEST_MATRIX_DUMP").is_ok() {
            eprintln!("--- isolated vector {} classified set (marker={marker:?}) ---", line.vector);
            for p in &iso_classified {
                eprintln!("{p}");
            }
        }
        let found = iso_classified.iter().any(|p| p.contains(&marker));
        assert!(
            found,
            "negative canary vector {}: run in isolation (every other vector skipped), no entry \
             containing {marker:?} appears in the shim's classified-paths set for that run — \
             this spelling was not recognised as under-root at all (outside-root, invisible to \
             every counter), which is exactly the failure mode this test exists to catch. \
             Spelling: {:?}, fixture-observed outcome (combined run): {}, note: {:?}. Isolated \
             classified set: {:?}",
            line.vector, line.spelling, line.outcome, line.note, iso_classified
        );
    }

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .expect("teardown");

    server.abort();
    if vector7_link_ready {
        let _ = std::fs::remove_dir(&vector7_link);
    }
}

/// The suffix `vfs-fixture-escape`'s **write-mode** vector 14 appends to the
/// canary's own path — mirrored from that crate's `V14_WRITE_SUFFIX`, which
/// documents why that one vector moves off the target in write mode.
///
/// This harness needs the name in order to *tolerate* it in the real-disk
/// listing, and only it. Vector 14's containment rests on the shim's
/// `CreateProcessInternalW` hook injecting the child (`vfs-shim/src/hook/process.rs`),
/// which fails closed — a child that cannot be injected is killed and never
/// runs unhooked. Observed here the child *is* injected and its write is
/// answered by the director like any other (see this file's
/// `negative_write_expectation` and the finding recorded in
/// `rust/docs/escape-matrix.md`), so this file does not normally appear on
/// disk at all. But a write that did reach the real disk would leave it there
/// through no fault of the canonicaliser, and treating that as a containment
/// failure would teach the next person to weaken the assertion. It is the one
/// name this harness accepts; anything else in the directory is an escape.
const V14_WRITE_SUFFIX: &str = ".v14-child-write.txt";

/// The alternate-stream name `vfs-fixture-escape`'s vector 11 builds. In
/// write mode that vector uses a creating disposition, so a spelling that
/// escapes leaves a real named stream on the canary — a create on real disk
/// that no directory listing would ever show.
const ADS_PROBE_STREAM: &str = "vfs-escape-fixture-probe";

/// The positive canary's expected outcome for a **write**, or `None` for a
/// vector this test does not assert an exact outcome for.
///
/// Same three exemptions the read matrix carries, for the same documented
/// reasons — `5b`'s construction fails at the NT level whatever the target,
/// and `13`/`14` are reported-not-closed in this gate. Every other buildable
/// spelling must come back `written`: opened for write, this vector's own
/// payload written, and that exact payload read back through the same
/// spelling (see `vfs-fixture-escape`'s module doc for why `written` is a
/// read-back claim and not merely "the call succeeded").
///
/// This half is not decoration. Gate 4 has already produced the failure it
/// guards against once — closing the write fall-through silently removed
/// copy-on-write while 526 tests stayed green. A containment matrix that only
/// asserted refusals would pass with every write broken.
fn positive_write_expectation(vector: &str) -> Option<&'static str> {
    if vector == "5b" || is_reported_not_closed(vector) {
        return None;
    }
    Some("written")
}

/// The negative canary's expected outcome for a **write**: `not-found`, for
/// every buildable spelling, with the same three exemptions as above.
///
/// Spec §8 criterion 1's load-bearing clause — "a write to the negative
/// canary must be **blocked**, and must not create a file on the real
/// filesystem under the root". The status half is this table; the filesystem
/// half is asserted separately, from this (uninjected) harness process, after
/// the run.
///
/// **Why the negative canary sits outside every mount here, rather than
/// inside a root mount whose backing directory merely lacks it** — the
/// construction the read matrix uses. A create is not a read: the read matrix
/// can put a writable `DiskProvider` at `/` and still call a file it does not
/// hold "unserved", because a read of an absent name is a refusal. A *create*
/// of that same name under a writable mount is something the provider graph
/// legitimately accepts and stores — contained, but not blocked. "A path no
/// provider serves" therefore means something stricter once writes are in
/// scope: no writable mount covers it at all. So the source here mounts at
/// `/Games/Skyrim/Data` and the negative canary lives in a sibling directory
/// under the same managed root, physically on disk, reachable by every one of
/// the fourteen spellings and owned by nothing.
fn negative_write_expectation(vector: &str) -> Option<&'static str> {
    if vector == "5b" || is_reported_not_closed(vector) {
        return None;
    }
    Some("not-found")
}


/// The only two names this harness accepts in a canary directory's real-disk
/// listing after a write run: the canary it put there itself, and vector 14's
/// injected child (see [`V14_WRITE_SUFFIX`]).
///
/// Vector 8's hardlink is deliberately **not** on this list. It is created by
/// `CreateHardLinkW`, which the shim does not hook by name — but the NT opens
/// underneath it are hooked, and observed behaviour is that the link either
/// never comes into existence on real disk (the negative canary, where
/// `hard_link` fails outright and the vector reports `unbuildable:`) or is
/// gone again by the end of the run. Accepting the name "just in case" would
/// mean a real-disk create could appear here forever without anyone noticing,
/// which is the opposite of what this function is for. If it ever does show
/// up, this test should fail and someone should find out why.
fn accounted_for_on_real_disk(name: &str, canary: &str) -> bool {
    name == canary || name == format!("{canary}{V14_WRITE_SUFFIX}")
}

/// Create a junction at a fresh temp path pointing at `target`, from this
/// (never-injected) harness process. See `VFS_ESCAPE_VECTOR7_LINK_DIR` in
/// `vfs-env` for why vector 7's junction must pre-date the fixture launch.
fn make_escape_junction(tag: &str, target: &Path) -> (PathBuf, Option<String>) {
    let link = std::env::temp_dir().join(format!("vfs-escape-junction-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir(&link);
    let ready = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J", &link.to_string_lossy(), &target.to_string_lossy()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let dir = ready.then(|| link.to_string_lossy().into_owned());
    (link, dir)
}

/// **The write half of the canary matrix** (gate 4, Task 8).
///
/// Spec §8 criterion 1 asks for the matrix green for *write* access as well
/// as read: 14 spellings × 2 canaries, unbuildable vectors reported as
/// unbuildable, and — the clause that carries the weight — "a write to the
/// negative canary must be **blocked**, and must not create a file on the
/// real filesystem under the root".
///
/// A separate test from the read matrix rather than a second pass inside it,
/// for a reason that is structural and not about runtime: the two need
/// different mount geometry. See `negative_write_expectation`'s doc comment —
/// a writable mount at `/` makes a create of *any* under-root name something
/// the provider graph accepts, so the read matrix's negative canary (a name
/// its backing directory merely lacks) is not unserved for writes at all. The
/// source here mounts at `/Games/Skyrim/Data`; the negative canary lives in a
/// sibling directory no mount covers.
///
/// **The real-filesystem assertions are the point, and they are made from
/// this process.** A write that is refused at the API while still leaving a
/// zero-byte file under the root has breached containment and reported
/// success. `vfs-directord`'s test harness is never injected, so `read_dir`,
/// `exists` and `read` here answer about physical disk — the equivalent of
/// `write_seal.rs`'s `drop(hooks)` before it inspects the root, and stronger,
/// because there is no detour in this process to drop. Four things are
/// checked after the negative run:
///
/// 1. the canary is present in its directory at all — the guard that makes
///    the three absences below mean something, because every one of them
///    would also hold against an empty lookalike directory;
/// 2. its bytes are byte-identical to what this harness wrote — no write
///    reached it, and no truncating open emptied it;
/// 3. that directory holds nothing besides the canary and the one artefact
///    `accounted_for_on_real_disk` tolerates — no spelling created a file;
/// 4. no named stream was created on it (vector 11 writes with a creating
///    disposition, and a stream is a create no directory listing shows).
///
/// See `assert_no_escaped_real_files` for the rest of what stops those
/// absences being vacuous: the writability probe below, the twenty-two asserted
/// outcome lines, and the standalone run of this same fixture that *does*
/// produce the artefacts 3 and 4 forbid.
///
/// The canaries sit in directories this harness proves physically writable
/// first, by creating and deleting a probe file in each. A "nothing was
/// created" assertion against a directory that could not be written to
/// anyway establishes nothing.
///
/// The positive canary is mirrored: identical seed bytes in the provider's
/// backing store *and* physically under the managed root. That pairing is
/// what makes both halves of its check meaningful — the writes must land in
/// the provider's copy (asserted on `content_dir`) and must not touch the
/// physical one (asserted on `session.root`), and every vector stays
/// buildable because the physical file the 8.3-name and hardlink
/// constructions need is really there.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn escape_matrix_write_access_positive_and_negative_canary() {
    let _guard = LAUNCH_LOCK.lock().await;
    ensure_inject_artifacts();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let registry = SessionRegistry::new();
    let svc = DirectorService::new(registry);
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let content_dir = tempfile::tempdir().expect("tempdir");
    let stats_dir = tempfile::tempdir().expect("stats tempdir");
    let stats_log = stats_dir.path().join("shim-stats.log");
    let out_dir = tempfile::tempdir().expect("out tempdir");
    let out_file = out_dir.path().join("escape-write-out.tsv");

    let fixture = locate_artifact("vfs-fixture-escape.exe");
    let mut client = connect(&format!("{addr}")).await.expect("connect");

    let session = client
        .create_session(vfs_control::pb::CreateSessionReq {
            name: "escape-matrix-write".into(),
        })
        .await
        .expect("CreateSession")
        .into_inner();
    assert!(!session.id.is_empty());
    assert!(!session.root.is_empty());

    use vfs_control::pb::{source_spec, AddSourceReq, DiskSource, SourceSpec as PbSource};

    // Mounted at a sub-path, deliberately — see the test's own doc comment.
    // Everything under `Games/Skyrim/Data` is served (and writable, since a
    // `DiskProvider` declares `Access::ReadWrite`); everything else under the
    // managed root is owned by no provider at all.
    const SERVED_MOUNT: &str = "/Games/Skyrim/Data";
    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(PbSource {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: content_dir.path().to_string_lossy().into_owned(),
                })),
            }),
            mount: SERVED_MOUNT.into(),
            layer: 0,
            root: 0,
            write_layer: false,
            cache_key: String::new(),
        })
        .await
        .expect("AddSource");

    let root = PathBuf::from(&session.root);
    let served_sub = PathBuf::from("Games").join("Skyrim").join("Data");
    let unserved_sub = PathBuf::from("Games").join("Skyrim").join("Unserved");
    std::fs::create_dir_all(root.join(&served_sub)).expect("mkdir served dir under session root");
    std::fs::create_dir_all(root.join(&unserved_sub)).expect("mkdir unserved dir under session root");

    // Both seeds are shorter than the fixture's fixed 22-byte write payload,
    // which matters: the write disposition is `OPEN_ALWAYS` (create, never
    // truncate), so a longer seed would leave a tail behind and every
    // read-back would mismatch for a reason unrelated to containment. See
    // `write_payload` in `vfs-fixture-escape`.
    const POSITIVE_BASENAME: &str = "escape-write-positive-canary.esp";
    const POSITIVE_SEED: &[u8] = b"pos-seed";
    const NEGATIVE_BASENAME: &str = "escape-write-negative-canary.bin";
    const NEGATIVE_SEED: &[u8] = b"neg-seed";
    /// Every write payload the fixture produces starts with this — see
    /// `write_payload`. Used to recognise "a canary write landed here"
    /// without pinning which vector wrote last.
    const PAYLOAD_PREFIX: &str = "vfs-escape-write[";

    let pos_on_disk = root.join(&served_sub).join(POSITIVE_BASENAME);
    let pos_in_provider = content_dir.path().join(POSITIVE_BASENAME);
    std::fs::write(&pos_on_disk, POSITIVE_SEED).expect("write positive canary (session root)");
    std::fs::write(&pos_in_provider, POSITIVE_SEED).expect("write positive canary (content dir)");

    let neg_on_disk = root.join(&unserved_sub).join(NEGATIVE_BASENAME);
    std::fs::write(&neg_on_disk, NEGATIVE_SEED).expect("write negative canary");

    // Both canary directories must be physically writable from here, or every
    // "nothing was created on real disk" assertion below is satisfied by the
    // filesystem rather than by containment.
    for dir in [root.join(&served_sub), root.join(&unserved_sub)] {
        let probe = dir.join(".harness-writability-probe");
        std::fs::write(&probe, b"x").unwrap_or_else(|e| {
            panic!(
                "{dir:?} must be physically writable for this test to prove anything — a create \
                 that could not have succeeded anyway is not evidence of containment: {e}"
            )
        });
        std::fs::remove_file(&probe).expect("remove writability probe");
    }

    // One junction per canary directory: vector 7 opens `<junction>\<target
    // filename>`, so a junction pointing at the served directory cannot serve
    // the unserved canary's run.
    let (pos_link, pos_link_dir) = make_escape_junction("write-pos", &root.join(&served_sub));
    let (neg_link, neg_link_dir) = make_escape_junction("write-neg", &root.join(&unserved_sub));

    // ---------------------------------------------------------------
    // Positive canary: every buildable spelling writes, and reads its own
    // payload back through the same spelling.
    // ---------------------------------------------------------------
    let pos_ctx = EscapeFixtureCtx {
        session_id: &session.id,
        fixture: &fixture,
        stats_log: &stats_log,
        vector7_link_dir: pos_link_dir.as_deref(),
        write_access: true,
    };
    let (pos_exit, pos_lines, _pos_classified, pos_truncated) =
        run_escape_fixture(&mut client, &pos_ctx, &pos_on_disk, &out_file, None).await;
    if std::env::var("VFS_TEST_MATRIX_DUMP").is_ok() {
        eprintln!("=== POSITIVE WRITE lines ===");
        for l in &pos_lines {
            eprintln!("{}\t{}\t{}\t{}", l.vector, l.spelling, l.outcome, l.note);
        }
    }
    assert_eq!(
        pos_exit, 0,
        "vfs-fixture-escape (write mode) must exit 0 against the positive canary — a crash exit \
         means a vector took the process down before the rest of the matrix was attempted. Lines \
         captured: {pos_lines:?}"
    );
    assert!(!pos_truncated, "the shim report's path list truncated on the positive write run");
    for id in ALL_VECTOR_IDS {
        assert!(
            pos_lines.iter().any(|l| &l.vector == id),
            "positive canary, write access: vector {id} produced no line at all in {out_file:?} — \
             a missing line must never be readable as a pass"
        );
    }
    for line in &pos_lines {
        let Some(want) = positive_write_expectation(&line.vector) else { continue };
        if line.outcome.starts_with("unbuildable:") {
            continue; // First-class, environment-dependent; recorded, not a failure.
        }
        assert_eq!(
            line.outcome, want,
            "positive canary, write access, vector {}: expected `{want}` — legitimate writes must \
             keep working through every spelling, or containment has been bought by breaking the \
             filesystem. Got `{}` (spelling: {:?}, note: {:?})",
            line.vector, line.outcome, line.spelling, line.note
        );
    }
    // The writes landed in the provider's store …
    let served_bytes = std::fs::read(&pos_in_provider).expect("read positive canary in provider");
    assert!(
        String::from_utf8_lossy(&served_bytes).starts_with(PAYLOAD_PREFIX),
        "the positive canary's writes must land where the provider graph says they land; \
         {pos_in_provider:?} still holds {:?}",
        String::from_utf8_lossy(&served_bytes)
    );
    // … and nowhere near the byte-identical physical file under the root.
    assert_eq!(
        std::fs::read(&pos_on_disk).expect("read positive canary on real disk"),
        POSITIVE_SEED,
        "a write reached the real file at {pos_on_disk:?} under the managed root. The provider \
         serves this vpath, so every write had somewhere legitimate to go — landing here instead \
         means a spelling escaped to disk"
    );
    assert_no_escaped_real_files(
        &root.join(&served_sub),
        POSITIVE_BASENAME,
        &pos_on_disk,
        "positive canary",
    );

    // ---------------------------------------------------------------
    // Negative canary: a real file under the managed root that no mount
    // covers. Every buildable spelling's write is refused, and real disk is
    // untouched.
    // ---------------------------------------------------------------
    let neg_ctx = EscapeFixtureCtx {
        session_id: &session.id,
        fixture: &fixture,
        stats_log: &stats_log,
        vector7_link_dir: neg_link_dir.as_deref(),
        write_access: true,
    };
    let (neg_exit, neg_lines, _neg_classified, neg_truncated) =
        run_escape_fixture(&mut client, &neg_ctx, &neg_on_disk, &out_file, None).await;
    if std::env::var("VFS_TEST_MATRIX_DUMP").is_ok() {
        eprintln!("=== NEGATIVE WRITE lines ===");
        for l in &neg_lines {
            eprintln!("{}\t{}\t{}\t{}", l.vector, l.spelling, l.outcome, l.note);
        }
    }
    assert_eq!(
        neg_exit, 0,
        "vfs-fixture-escape (write mode) must exit 0 against the negative canary too. Lines \
         captured: {neg_lines:?}"
    );
    assert!(!neg_truncated, "the shim report's path list truncated on the negative write run");
    for id in ALL_VECTOR_IDS {
        assert!(
            neg_lines.iter().any(|l| &l.vector == id),
            "negative canary, write access: vector {id} produced no line at all in {out_file:?}"
        );
    }
    for line in &neg_lines {
        let Some(want) = negative_write_expectation(&line.vector) else { continue };
        if line.outcome.starts_with("unbuildable:") {
            continue; // Never attempted at the OS level; nothing to seal.
        }
        assert_eq!(
            line.outcome, want,
            "negative canary, write access, vector {}: expected `{want}` — a write to a path \
             under the managed root that no provider serves must be blocked, for every buildable \
             spelling. Got `{}` (spelling: {:?}, note: {:?})",
            line.vector, line.outcome, line.spelling, line.note
        );
    }
    // The filesystem half of spec §8 criterion 1, asserted on real disk from
    // this never-injected process.
    assert_eq!(
        std::fs::read(&neg_on_disk).expect("read negative canary on real disk"),
        NEGATIVE_SEED,
        "the negative canary's real bytes at {neg_on_disk:?} changed. A refusal at the API that \
         still modifies the file under the root is the breach this whole matrix exists to catch"
    );
    assert_no_escaped_real_files(
        &root.join(&unserved_sub),
        NEGATIVE_BASENAME,
        &neg_on_disk,
        "negative canary",
    );

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .expect("teardown");

    server.abort();
    let _ = std::fs::remove_dir(&pos_link);
    let _ = std::fs::remove_dir(&neg_link);
}

/// The real-filesystem half of the write matrix, for one canary: nothing was
/// created in its directory and no named stream was created on it.
///
/// **Called with the detours nowhere in sight.** This is the `vfs-directord`
/// test process, which is never injected, so every `std::fs` call here reads
/// physical disk — the same ordering `write_seal.rs` gets by doing
/// `drop(hooks)` before it inspects the root, and stronger, because there is
/// no detour in this process to drop in the first place. A hook-live
/// `exists()` answers about the provider graph, which is exactly the answer a
/// breached containment layer would want it to give.
///
/// **What stops these absences from being vacuous**, in order:
///
/// - the canary itself must be in the listing, so this is provably the
///   physical directory the run targeted and not an empty lookalike;
/// - the caller proved that directory physically writable, by creating and
///   deleting a probe file in it before the run, so a create here genuinely
///   could have succeeded;
/// - the fixture reported an attempted spelling for all twenty-two lines, each
///   naming a path in this directory, and the caller asserted on every one of
///   them;
/// - and the same fixture in the same write mode, run standalone against an
///   ordinary directory, *does* create `<name>.`, `<name> ` and the named
///   stream (vectors 10b, 10c and 11 with a creating disposition). These
///   assertions have teeth; they were watched failing before they were
///   watched passing.
fn assert_no_escaped_real_files(dir: &Path, canary: &str, canary_path: &Path, label: &str) {
    let names = real_dir_names(dir);

    assert!(
        names.iter().any(|n| n == canary),
        "{label}: {canary:?} is not in {dir:?} at all ({names:?}) — this harness is not looking \
         at the physical directory the run targeted, so every `nothing was created` assertion \
         below would pass for the wrong reason"
    );

    let stray: Vec<&String> =
        names.iter().filter(|n| !accounted_for_on_real_disk(n, canary)).collect();
    assert!(
        stray.is_empty(),
        "{label}: these files appeared on the REAL filesystem under the managed root at {dir:?}: \
         {stray:?}. Every one of them is a spelling whose write was answered by disk instead of \
         by the director — spec §8 criterion 1's `must not create a file on the real filesystem \
         under the root`. (Trailing-dot and trailing-space spellings show up here as names that \
         look identical to the canary's; a doubled entry is vector 10b or 10c escaping.)"
    );

    let stream = format!("{}:{ADS_PROBE_STREAM}", canary_path.display());
    assert!(
        std::fs::File::open(&stream).is_err(),
        "{label}: vector 11 created the named stream {stream:?} on the real file under the \
         managed root. A stream is a create that no directory listing shows, which is exactly \
         why it is checked by name rather than left to the listing above"
    );
}

/// Stage 2b exit criterion: **the escape matrix passes against every root,
/// not just the first.**
///
/// `escape_matrix_positive_and_negative_canary` above proves containment for
/// root 0 — the session's own root, the one the daemon creates and the one
/// every path in this tree used to be measured against. That proves nothing
/// about a second root, and the failure it would miss is not subtle: the
/// canonicaliser could have a root-index assumption baked into it (matching
/// only `roots[0]`, or resolving device/junction aliases against root 0's
/// path alone) and root 0's matrix would stay green while every path under
/// root 1 fell through to real disk, unclassified and uncounted.
///
/// So this runs the same fixture, the same two canaries, and the same
/// `positive_expectation`/`negative_expectation` tables against a target
/// under **root 1**: a second real host directory, declared with
/// `SessionRegistry::declare_root` and served by its own provider mounted at
/// `RootId(1)` through the ordinary `AddSourceReq { root: 1 }` path.
///
/// It exercises the whole chain end to end and nothing about it is stubbed:
/// the daemon publishes root 1 into `VFS_VIRTUAL_ROOTS`, the shim's
/// `RootMap` holds both roots, `vpath_under_root` answers `RootId(1)`, the
/// ring payload carries that 1, and `dispatch_director` routes on it. Any
/// link missing turns the positive canary's ordinary spelling into
/// `not-found`, which is what makes this worth its runtime rather than a
/// duplicate of the root-0 run.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn escape_matrix_holds_against_a_second_root() {
    let _guard = LAUNCH_LOCK.lock().await;
    ensure_inject_artifacts();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let registry = SessionRegistry::new();
    // Cloned before the service takes it: `declare_root` has no RPC of its own
    // (a root's *host path* comes from a config's `[[root]] path`, and
    // `AddSourceReq` carries a root id and no path), so the test declares it
    // the same way a config-driven daemon would. Everything else here — the
    // session, the source on root 1, the launch — goes over gRPC.
    let reg_handle = registry.clone();
    let svc = DirectorService::new(registry);
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Root 1's own host directory — the "Documents\My Games\Skyrim" shape —
    // and its own backing content dir, deliberately separate so the negative
    // canary is a real file under root 1 that root 1's provider does not have.
    let docs_root = tempfile::tempdir().expect("docs root tempdir");
    let docs_content = tempfile::tempdir().expect("docs content tempdir");
    let stats_dir = tempfile::tempdir().expect("stats tempdir");
    let stats_log = stats_dir.path().join("shim-stats.log");
    let out_dir = tempfile::tempdir().expect("out tempdir");
    let out_file = out_dir.path().join("escape-root1-out.tsv");

    let fixture = locate_artifact("vfs-fixture-escape.exe");
    let mut client = connect(&format!("{addr}")).await.expect("connect");

    let session = client
        .create_session(vfs_control::pb::CreateSessionReq {
            name: "escape-matrix-root1".into(),
        })
        .await
        .expect("CreateSession")
        .into_inner();

    use vfs_control::pb::{source_spec, AddSourceReq, DiskSource, SourceSpec as PbSource};

    // Root 0 still gets a provider: a session whose game directory serves
    // nothing is not the shape being tested, and leaving it unmounted would
    // let a root-0 regression hide here.
    let game_content = tempfile::tempdir().expect("game content tempdir");
    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(PbSource {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: game_content.path().to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 0,
            root: 0,
            write_layer: false,
            cache_key: String::new(),
        })
        .await
        .expect("AddSource root 0");

    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(PbSource {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: docs_content.path().to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 0,
            root: 1,
            write_layer: false,
            cache_key: String::new(),
        })
        .await
        .expect("AddSource root 1");

    reg_handle
        .declare_root(&session.id, 1, docs_root.path(), "docs")
        .expect("declare root 1");

    let sub = PathBuf::from("Saves");
    std::fs::create_dir_all(docs_root.path().join(&sub)).expect("mkdir under root 1");
    std::fs::create_dir_all(docs_content.path().join(&sub)).expect("mkdir under root 1 content");

    // Same two-canary construction as the root-0 matrix, one root over.
    const POSITIVE_BASENAME: &str = "escape-positive-canary.esp";
    const POSITIVE_BYTES: &[u8] = b"the-positive-canary-bytes";
    let pos_rel = sub.join(POSITIVE_BASENAME);
    std::fs::write(docs_root.path().join(&pos_rel), POSITIVE_BYTES).expect("positive (root 1)");
    std::fs::write(docs_content.path().join(&pos_rel), POSITIVE_BYTES)
        .expect("positive (root 1 content)");

    const NEGATIVE_BASENAME: &str = "escape-negative-canary.bin";
    let neg_rel = sub.join(NEGATIVE_BASENAME);
    std::fs::write(docs_root.path().join(&neg_rel), b"the-negative-canary-bytes")
        .expect("negative (root 1)");

    // Vector 7's junction, created by this never-injected harness process for
    // the same reason the root-0 matrix does it here — pointed at root 1's
    // own directory, which is the part that would break if junction aliases
    // were resolved against root 0's path alone.
    let vector7_link =
        std::env::temp_dir().join(format!("vfs-escape-junction-root1-{}", std::process::id()));
    let _ = std::fs::remove_dir(&vector7_link);
    let vector7_link_ready = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &vector7_link.to_string_lossy(),
            &docs_root.path().join(&sub).to_string_lossy(),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let vector7_link_dir = vector7_link_ready.then(|| vector7_link.to_string_lossy().into_owned());
    let ctx = EscapeFixtureCtx {
        session_id: &session.id,
        fixture: &fixture,
        stats_log: &stats_log,
        vector7_link_dir: vector7_link_dir.as_deref(),
        write_access: false,
    };

    let (pos_exit, pos_lines, _, _) = run_escape_fixture(
        &mut client,
        &ctx,
        &docs_root.path().join(&pos_rel),
        &out_file,
        None,
    )
    .await;
    assert_eq!(
        pos_exit, 0,
        "vfs-fixture-escape must exit 0 against root 1's positive canary. Lines: {pos_lines:?}"
    );
    for id in ALL_VECTOR_IDS {
        assert!(
            pos_lines.iter().any(|l| &l.vector == id),
            "root 1 positive canary: vector {id} produced no line at all — a missing line must \
             never be readable as a pass"
        );
    }
    for line in &pos_lines {
        let Some(want) = positive_expectation(&line.vector) else { continue };
        if line.outcome.starts_with("unbuildable:") {
            continue;
        }
        assert_eq!(
            line.outcome, want,
            "root 1 positive canary, vector {}: expected `{want}`, got `{}` (spelling: {:?}, \
             note: {:?}). A blanket `not-found` here means root 1 never reached the director at \
             all — the shim did not learn the root, or the ring did not carry it.",
            line.vector, line.outcome, line.spelling, line.note
        );
    }

    let (neg_exit, neg_lines, _, _) = run_escape_fixture(
        &mut client,
        &ctx,
        &docs_root.path().join(&neg_rel),
        &out_file,
        None,
    )
    .await;
    assert_eq!(
        neg_exit, 0,
        "vfs-fixture-escape must exit 0 against root 1's negative canary. Lines: {neg_lines:?}"
    );
    for line in &neg_lines {
        let Some(want) = negative_expectation(&line.vector) else { continue };
        if line.outcome.starts_with("unbuildable:") {
            continue;
        }
        assert_eq!(
            line.outcome, want,
            "root 1 negative canary, vector {}: expected `{want}`, got `{}` (spelling: {:?}, \
             note: {:?}). `opened` here means a real file under root 1 that no provider serves \
             is still reachable — containment holds for root 0 and not for root 1.",
            line.vector, line.outcome, line.spelling, line.note
        );
    }

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .expect("teardown");

    server.abort();
    if vector7_link_ready {
        let _ = std::fs::remove_dir(&vector7_link);
    }
}

/// **The gap this test recorded is closed, and this is the flip.** It was
/// `documents_metadata_gap_for_unrecognised_spellings`, and it asserted
/// `found`.
///
/// What it recorded: Fix 2(b) from the final whole-branch review of Gate 3
/// found `docs/escape-matrix.md`'s claim of containment for metadata queries
/// "by the same `RootMap::decide` mechanism... regardless of which hook
/// asked" to be false. `qattr_hook`/`qfull_hook`/`qibn_hook`
/// (`vfs-shim/src/hook/file_attr.rs`) never reach `RootMap::decide` at all — they
/// consult `fuse_path_attr`, which asked `FuseClient::vpath_under_root`,
/// the *client's own* string-prefix predicate. That predicate had none of
/// `RootMap::compute_under_root`'s canonicalisation tables (no
/// device-prefix, volume-GUID, `GLOBALROOT`-unwrap, UNC-admin-share, or
/// junction-alias resolution), so five alternate spellings of an in-root
/// path were classified by one predicate and never routed by the other —
/// and a name-based attribute query on one of them reached real disk, even
/// though the matching *read open* on the identical spelling (vector 4
/// itself) was already sealed.
///
/// What changed: stage 2b task 5 **deleted the second predicate**.
/// `FuseClient` now holds a real `RootMap` — several roots, plus the staged
/// launch directory as an alias for root 0 — and `vpath_under_root` is that
/// map's canonicalising `resolve`, so there is one predicate rather than two
/// that can drift. The volume-GUID spelling this test builds is now
/// recognised by the client, routed to the director, and — since the
/// negative canary is a real file on `session.root` that no provider serves
/// — answered `not-found` rather than handed to real disk.
///
/// Kept rather than deleted, and kept in its original shape, because it is
/// the only end-to-end evidence that the unification reaches this hook
/// family: it launches `vfs-fixture-escape`'s opt-in `4m` vector
/// (`GetFileAttributesW` against vector 4's own volume-GUID spelling) under
/// a real, composed session, against a real on-disk negative canary. Revert
/// the unification and this assertion fails again, which is what makes it
/// worth its runtime.
///
/// **If this ever reads `found` again**, the client predicate has lost its
/// canonicalisation. Do not relax the assertion — find what stopped
/// consulting `RootMap`.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn metadata_queries_are_sealed_for_canonicaliser_only_spellings() {
    let _guard = LAUNCH_LOCK.lock().await;
    ensure_inject_artifacts();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let registry = SessionRegistry::new();
    let svc = DirectorService::new(registry);
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // The DiskProvider's backing store — deliberately NOT session.root, same
    // shape as the escape matrix test's own negative canary: a real file
    // under the managed root that this provider genuinely does not have.
    let content_dir = tempfile::tempdir().expect("tempdir");
    let stats_dir = tempfile::tempdir().expect("stats tempdir");
    let stats_log = stats_dir.path().join("shim-stats.log");
    let out_dir = tempfile::tempdir().expect("out tempdir");
    let out_file = out_dir.path().join("metadata-gap-out.tsv");

    let fixture = locate_artifact("vfs-fixture-escape.exe");
    let mut client = connect(&format!("{addr}")).await.expect("connect");

    let session = client
        .create_session(vfs_control::pb::CreateSessionReq {
            name: "metadata-gap".into(),
        })
        .await
        .expect("CreateSession")
        .into_inner();
    assert!(!session.id.is_empty());
    assert!(!session.root.is_empty());

    use vfs_control::pb::{source_spec, AddSourceReq, DiskSource, SourceSpec as PbSource};

    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(PbSource {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: content_dir.path().to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 0,
            root: 0,
            write_layer: false,
            cache_key: String::new(),
        })
        .await
        .expect("AddSource");

    let root = PathBuf::from(&session.root);
    let sub = PathBuf::from("Games").join("Skyrim").join("Data");
    std::fs::create_dir_all(root.join(&sub)).expect("mkdir under session root");

    // Negative canary: real bytes ONLY on session.root — identical
    // construction to `escape_matrix_positive_and_negative_canary`'s own.
    const NEGATIVE_BASENAME: &str = "escape-negative-canary.bin";
    let neg_rel = sub.join(NEGATIVE_BASENAME);
    std::fs::write(root.join(&neg_rel), b"the-negative-canary-bytes")
        .expect("write negative canary");

    let ctx = EscapeFixtureCtx {
        session_id: &session.id,
        fixture: &fixture,
        stats_log: &stats_log,
        vector7_link_dir: None,
        write_access: false,
    };

    let (exit, lines, _classified, _truncated) =
        run_escape_fixture(&mut client, &ctx, &root.join(&neg_rel), &out_file, Some("4m")).await;

    assert_eq!(
        exit, 0,
        "vfs-fixture-escape (isolated vector 4m) must exit 0. Lines captured: {lines:?}"
    );
    let line = lines
        .iter()
        .find(|l| l.vector == "4m")
        .unwrap_or_else(|| panic!("vector 4m produced no line at all in {out_file:?}"));

    if line.outcome.starts_with("unbuildable:") {
        panic!(
            "vector 4's own construction ({}) failed in this environment, so this test cannot \
             exercise the metadata-gap claim here — see vector 4's own `unbuildable` reasons in \
             `docs/escape-matrix.md`. This is an environment limitation, not evidence the gap is \
             closed.",
            line.outcome
        );
    }

    // The headline assertion, and the point of this test: a name-based
    // attribute query on the negative canary, via a spelling only
    // `RootMap`'s canonicaliser ever recognised, is sealed now that the
    // client predicate IS that canonicaliser.
    assert_eq!(
        line.outcome, "not-found",
        "expected the metadata query on the negative canary (via vector 4's volume-GUID \
         spelling: {:?}) to be sealed now that `fuse_client::vpath_under_root` is `RootMap`'s \
         own canonicalising predicate rather than a string-prefix test — got `{}` instead \
         (note: {:?}). `found` here means the client predicate lost its canonicalisation and \
         the qattr_hook/qfull_hook/qibn_hook family is reaching real disk again.",
        line.spelling, line.outcome, line.note
    );

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .expect("teardown");

    server.abort();
}
