//! Directory enumeration under a managed root hides an unserved real file.

// Every test here injects a real Windows process, so on other hosts the helpers are unused.
#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tokio::net::TcpListener;
use tonic::transport::Server;
use vfs_control::pb::director_server::DirectorServer;
use vfs_directord::{connect, DirectorService, SessionRegistry};

mod support;
use support::{escape::*, launch::*, artifacts::*};

/// Gate 4, Task 8b: **a directory listing under a managed root never reveals
/// a real, unserved file.**
///
/// Every other assertion in this file is about an *open* — can this spelling
/// reach these bytes. Enumeration is a different mechanism with a different
/// predicate: it runs on an already-opened handle, through
/// `NtQueryDirectoryFile(Ex)` → `vfs-shim::hook::serve_dir_query`, and asks
/// `FuseClient::vpath_under_root` rather than `RootMap::decide`. For two
/// gates `rust/docs/escape-matrix.md` claimed enumeration containment
/// "follows directly from read-open containment rather than being a separate
/// mechanism". It does not follow, and behind the argument sat a branch that
/// drained the real directory behind the mount and listed whatever was
/// physically there.
///
/// **That branch was latent, not live** — reachable only if a second,
/// unrelated seal (gate 3 task 5's `NotFound` deny) regressed alongside a
/// disagreement between the shim's two under-root predicates. Both are needed;
/// see the mutation note below. This test's value is that enumeration no
/// longer depends on another gate's invariant untested, not that it rescued a
/// session from an active leak.
///
/// Nothing tested either branch: no fixture called `read_dir` under a session
/// at all, the shim-level enumeration tests attach no director, and every
/// director-level `readdir` test asserts presence or de-duplication, never
/// absence.
///
/// This test is the measurement that replaced the argument. It runs the
/// escape fixture's `enum` vector under a real composed session — daemon,
/// director, injected shim — against the same two-canary geometry the read
/// matrix uses, and asserts **both** directions of one listing:
///
/// - the provider-served positive canary **appears** (so the test cannot pass
///   by breaking enumeration outright, which is how a one-sided
///   absence assertion would pass), and
/// - the physically-present, unserved negative canary **does not**.
///
/// It also asserts the instrument, not only the bytes: `hookstats`'s
/// per-enumeration record must name `director` as the source for this
/// directory. That flag existed before this task (as a `served: bool` that
/// read `true` for the real-disk branch too) and nothing anywhere asserted
/// it. A regression that reinstated a real-disk listing while the director
/// happened to serve the same names would still be caught by the source
/// column even if the bytes agreed.
///
/// **Proven able to fail, not merely observed passing.** With
/// `FuseClient::vpath_under_root` mutated to decline this directory and
/// `RootMap::decide`'s `NotFound` arm reverted to the pre-gate-3
/// `PassThrough` — the two states, both historically real in this tree, that
/// together produce a real directory handle the client does not claim — the
/// unserved canary appears in the listing and this test fails on its absence
/// assertion. Neither mutation alone suffices, which is the measurement
/// behind "latent, not live" above. See `task-8b-report.md` for both
/// mutations and their output.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn directory_enumeration_under_a_managed_root_hides_an_unserved_real_file() {
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

    // Same geometry as the read matrix: the provider's backing store is a
    // separate directory, so a file written only onto `session.root` is a
    // real file under the managed root that no provider serves.
    let content_dir = tempfile::tempdir().expect("tempdir");
    let stats_dir = tempfile::tempdir().expect("stats tempdir");
    let stats_log = stats_dir.path().join("shim-stats.log");
    let out_dir = tempfile::tempdir().expect("out tempdir");
    let out_file = out_dir.path().join("escape-enum-out.tsv");

    let fixture = locate_artifact("vfs-fixture-escape.exe");
    let mut client = connect(&format!("{addr}")).await.expect("connect");

    let session = client
        .create_session(vfs_control::pb::CreateSessionReq {
            name: "escape-enum".into(),
        })
        .await
        .expect("CreateSession")
        .into_inner();

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

    const SERVED: &str = "enum-served-canary.esp";
    const UNSERVED: &str = "enum-unserved-canary.bin";
    let served_rel = sub.join(SERVED);
    std::fs::write(root.join(&served_rel), b"served").expect("write served canary (root)");
    std::fs::write(content_dir.path().join(&served_rel), b"served")
        .expect("write served canary (content_dir)");
    std::fs::write(root.join(sub.join(UNSERVED)), b"unserved").expect("write unserved canary");

    // The listing must be a real question of containment, not of existence:
    // both files really are in the physical directory this vector lists, and
    // an uninjected reader of that directory really does see both. Asserted
    // from this (never-injected) harness process before the fixture runs, so
    // "the unserved canary was absent" can never mean "it was never there".
    let physical = real_dir_names(&root.join(&sub));
    assert!(
        physical.iter().any(|n| n.eq_ignore_ascii_case(SERVED))
            && physical.iter().any(|n| n.eq_ignore_ascii_case(UNSERVED)),
        "the physical directory must hold both canaries before the fixture runs, or the \
         absence assertion below proves nothing: {physical:?}"
    );

    let ctx = EscapeFixtureCtx {
        session_id: &session.id,
        fixture: &fixture,
        stats_log: &stats_log,
        vector7_link_dir: None,
        write_access: false,
    };
    let (exit, lines, _classified, _truncated) = run_escape_fixture(
        &mut client,
        &ctx,
        &root.join(&served_rel),
        &out_file,
        Some("enum"),
    )
    .await;

    assert_eq!(exit, 0, "the enumeration fixture must exit 0. Lines: {lines:?}");
    let line = lines
        .iter()
        .find(|l| l.vector == "enum")
        .unwrap_or_else(|| panic!("no `enum` line in {out_file:?}: {lines:?}"));
    assert!(
        line.outcome.starts_with("listed:"),
        "the served directory must be enumerable under a session — a listing that failed \
         outright would make the absence assertion below vacuous. Got `{}` (note: {:?})",
        line.outcome,
        line.note
    );

    let listed: Vec<String> = line.note.split('|').filter(|s| !s.is_empty()).collect::<Vec<_>>()
        .into_iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();
    assert!(
        listed.iter().any(|n| n == &SERVED.to_ascii_lowercase()),
        "the provider-served file is missing from the listing — enumeration is broken, and a \
         broken enumeration would satisfy the absence half of this test for free. Listed: \
         {listed:?}"
    );
    assert!(
        !listed.iter().any(|n| n == &UNSERVED.to_ascii_lowercase()),
        "a real, physically-present file under the managed root that no provider serves \
         appeared in a directory listing. Reads, metadata and writes are all sealed for this \
         file; enumeration handed out its name. Listed: {listed:?}"
    );

    // The instrument, not only the bytes. `serve_dir_query` records which
    // mechanism produced each listing; the served directory's rows must all
    // say `director`. A regression to the real-disk drain shows up here even
    // in a scenario where the names happen to agree.
    //
    // The recorded path is the NT spelling the open carried, which for a
    // directory keeps its trailing separator — matched on the trimmed form so
    // this does not turn into a silently-vacuous filter if that ever changes.
    let want_dir = format!("\\{}", sub.to_string_lossy().to_ascii_lowercase());
    let records = support::readdir_records(&stats_log);
    let ours: Vec<&support::ReadDirRecord> = records
        .iter()
        .filter(|r| r.dir.trim_end_matches('\\').ends_with(&want_dir))
        .collect();
    assert!(
        !ours.is_empty(),
        "the shim recorded no enumeration of {want_dir:?} at all, so the source assertion \
         below would pass vacuously. All records: {records:?}"
    );
    for r in &ours {
        assert_eq!(
            r.source, "director",
            "a listing of a directory under the managed root was produced by `{}`, not the \
             director. `contained` means the shim's two under-root predicates disagree about \
             this path; anything else means real disk answered. Record: {r:?}",
            r.source
        );
    }
    assert!(
        ours.iter().any(|r| r.count > 0),
        "every recorded listing of {want_dir:?} came back with zero entries: {ours:?}"
    );

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .expect("teardown");
    server.abort();
}
