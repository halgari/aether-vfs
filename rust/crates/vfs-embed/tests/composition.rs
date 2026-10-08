//! Composition through the incremental-host surface: sources added one at a
//! time with [`RootSources`] and installed with `Session::set_root_mounts`,
//! the shape a host that learns its sources from a config or a UI builds.
//! Portable, no inject.
//!
//! Ported from the removed daemon's `composition.rs` and `session_config.rs`:
//! the assertions about what a composed session serves are kept; the ones
//! about the daemon's gRPC surface (its `Stats` session count, `AddSource` on
//! the wire, TOML loading) went with it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use vfs_embed::{DiskProvider, Provider, RootId, RootSources, Session, OPEN_READ};
use vfs_testkit::zip::write_stored_zip;

/// A one-entry Stored zip named `layer.zip` inside `dir`.
fn layer_zip(dir: &Path, entry: &str, content: &[u8]) -> std::path::PathBuf {
    let path = dir.join("layer.zip");
    write_stored_zip(&path, entry, content);
    path
}

/// A session whose directories live under `base`, so nothing it creates lands
/// in the host's temp dir.
fn session_in(base: &Path) -> Session {
    let mut s = Session::new();
    s.set_root(base.join("root"));
    s.set_overlay(base.join("overlay"));
    s.set_state_dir(base.join("state"));
    s
}

/// Record one source for `root` and reinstall that root's whole mount list —
/// what a host adding sources one at a time does on every source, because
/// `Director` holds one provider per root.
fn add_source(
    s: &Session,
    sources: &mut RootSources,
    root: u32,
    mount: &str,
    layer: i32,
    provider: Arc<dyn Provider>,
) {
    sources.add(mount, layer, provider);
    s.set_root_mounts(RootId(root), sources.mounts().expect("compose root"))
        .unwrap_or_else(|st| panic!("mount root {root} status {st}"));
}

#[test]
fn registry_layered_disk_sources_top_wins() {
    let base = vfs_testkit::tempdir().unwrap();
    let mod_dir = vfs_testkit::tempdir().unwrap();
    let session_dir = vfs_testkit::tempdir().unwrap();
    std::fs::write(base.path().join("shared.txt"), b"FROM-BASE").unwrap();
    std::fs::write(base.path().join("only-base.txt"), b"BASE").unwrap();
    std::fs::write(mod_dir.path().join("shared.txt"), b"MOD-WIN").unwrap();
    std::fs::write(mod_dir.path().join("only-mod.txt"), b"MOD").unwrap();

    let session = session_in(session_dir.path());
    let mut root0 = RootSources::new();
    add_source(
        &session,
        &mut root0,
        0,
        "/",
        0,
        Arc::new(DiskProvider::new(base.path())),
    );
    add_source(
        &session,
        &mut root0,
        0,
        "/",
        10,
        Arc::new(DiskProvider::new(mod_dir.path())),
    );

    let shared = session.read_file("shared.txt").unwrap();
    assert_eq!(shared, b"MOD-WIN");
    let only_base = session.read_file("only-base.txt").unwrap();
    assert_eq!(only_base, b"BASE");
    let only_mod = session.read_file("only-mod.txt").unwrap();
    assert_eq!(only_mod, b"MOD");
}

#[test]
fn registry_zip_source_reads_entry() {
    let dir = vfs_testkit::tempdir().unwrap();
    let session_dir = vfs_testkit::tempdir().unwrap();
    let zip = layer_zip(dir.path(), "Data/proof.dat", b"ZIP-BYTES");
    let session = session_in(session_dir.path());
    let mut root0 = RootSources::new();
    add_source(
        &session,
        &mut root0,
        0,
        "/",
        0,
        Arc::new(vfs_zip::ZipProvider::open(&zip).expect("zip index")),
    );
    let got = session.read_file("Data/proof.dat").unwrap();
    assert_eq!(got, b"ZIP-BYTES");
}

/// Historical note: this test used to demonstrate two confirmed gaps in
/// non-root mount support (see `escape-matrix.md`'s "The Mod Organizer
/// consequence" section for the full history) — mount-prefix matching was
/// case-sensitive while every shim vpath is always lowercased, and
/// `Director::readdir` never surfaced a mount registered below the queried
/// directory, so it could be opened by a known path but never discovered by
/// listing its parent. Stage 2b task 1 (`vfs-director::path::strip_prefix`,
/// `vfs-director::director::Director::readdir`) closed both: prefix
/// comparison now folds ASCII case on both sides, and `readdir` contributes
/// the next path component of any deeper mount as a synthetic directory
/// entry. This test now asserts the fixed behavior directly, including a
/// mixed-case mount (the original, `escape-matrix.md`-documented spelling)
/// to prove case is no longer a live concern either.
#[test]
fn non_root_mount_matches_lowercase_open_and_is_discoverable_via_parent_readdir() {
    let root_dir = vfs_testkit::tempdir().unwrap();
    let session_dir = vfs_testkit::tempdir().unwrap();
    // A real, physical "Data" directory the root disk mount can enumerate,
    // standing in for the base game content a real session always has.
    let data_dir = root_dir.path().join("Data");
    std::fs::create_dir(&data_dir).unwrap();
    std::fs::write(data_dir.join("Skyrim.esm"), b"BASE-CONTENT").unwrap();

    // The mod's staging directory — physically anywhere else entirely, never
    // nested under `root_dir`, exactly the MO2 shape the matrix documents.
    let mod_dir = vfs_testkit::tempdir().unwrap();
    std::fs::write(mod_dir.path().join("foo.esp"), b"MOD-BYTES").unwrap();

    let session = session_in(session_dir.path());
    let mut root0 = RootSources::new();
    add_source(
        &session,
        &mut root0,
        0,
        "/",
        0,
        Arc::new(DiskProvider::new(root_dir.path())),
    );
    // Mixed case, deliberately — the original `escape-matrix.md`-documented
    // spelling. Case folding at compare time means this must match a
    // lowercased live open exactly as a lowercase-authored mount would.
    add_source(
        &session,
        &mut root0,
        0,
        "Data/SomeMod",
        10,
        Arc::new(DiskProvider::new(mod_dir.path())),
    );

    // A direct open by a known relative path succeeds through the
    // non-root mount, even though the mount was registered mixed-case
    // and the open is spelled all-lowercase (what the shim always sends).
    let bytes = session.read_file("data/somemod/foo.esp").unwrap();
    assert_eq!(bytes, b"MOD-BYTES");

    // The base content is still there and enumerable...
    let base_entries = session.kernel().readdir(RootId::DEFAULT, "data").unwrap();
    assert!(
        base_entries
            .iter()
            .any(|e| e.name.eq_ignore_ascii_case("Skyrim.esm")),
        "expected the real base content to still enumerate: {:?}",
        base_entries.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
    // ...and the mount point itself now appears as a synthetic child
    // entry too: the gap this test used to demonstrate is closed.
    assert!(
        base_entries
            .iter()
            .any(|e| e.name.eq_ignore_ascii_case("somemod")),
        "expected readdir(\"data\") to list the non-root mount point as \
         a synthetic child entry: {:?}",
        base_entries.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
}

/// The measurement gate's director-side exposure: the director's own open
/// counts (`vfs_embed::open_totals`) must reflect a served open. The removed
/// daemon reported them on its `Stats` RPC; the number itself is the
/// session's, and is what is asserted here. Drives a real open through
/// `dispatch_director` (the same function the ring calls for `OP_OPEN`)
/// rather than `Session::read_file`, since `read_file` goes straight through
/// the kernel and never touches `io_stats::record_open`.
#[test]
fn stats_rpc_reports_open_counts_after_session_activity() {
    let dir = vfs_testkit::tempdir().unwrap();
    let session_dir = vfs_testkit::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), b"hi").unwrap();
    let session = session_in(session_dir.path());
    let mut root0 = RootSources::new();
    add_source(
        &session,
        &mut root0,
        0,
        "/",
        0,
        Arc::new(DiskProvider::new(dir.path())),
    );

    let (before_ok, _) = vfs_embed::open_totals();
    let (st, payload) = vfs_director::ring_dispatch::dispatch_director(
        session.kernel(),
        vfs_protocol::OP_OPEN,
        &vfs_protocol::encode_open_req(0, OPEN_READ, "f.txt"),
        0,
        4096,
        None,
    );
    assert_eq!(st, vfs_protocol::ST_OK, "the served open must succeed");
    assert!(vfs_protocol::decode_open_resp(&payload).is_some());

    let (after_ok, _) = vfs_embed::open_totals();
    assert!(
        after_ok > before_ok,
        "opens_ok did not reflect the served open: before={before_ok} after={after_ok}"
    );
}

/// A two-root session's declared root paths reach the live session, and both
/// roots serve their own provider.
///
/// Ported from the daemon's `a_configs_declared_root_paths_reach_the_live_session`:
/// a config's `[[root]]` table once carried an id and no path, so a two-root
/// session mounted both providers correctly and the shim learned about
/// exactly one root — every path under the second falling through to real
/// disk with nothing reporting it. What a config did there is what a host
/// does here: `declare_root` per root, then a source per root.
///
/// Asserted at the session: the point is that the value arrives somewhere
/// that `Session::launch` will publish.
#[test]
fn a_configs_declared_root_paths_reach_the_live_session() {
    let game = vfs_testkit::tempdir().unwrap();
    let docs = vfs_testkit::tempdir().unwrap();
    let session_dir = vfs_testkit::tempdir().unwrap();
    std::fs::write(game.path().join("a.txt"), b"g").unwrap();
    std::fs::write(docs.path().join("a.txt"), b"d").unwrap();
    // Each root's location: where the program sees it. On Windows that is a
    // host directory (here the source directory itself, as before); on Linux
    // a `C:\…` path inside the Wine prefix — a host path is refused there.
    let (game_loc, docs_loc) = if cfg!(windows) {
        (game.path().to_path_buf(), docs.path().to_path_buf())
    } else {
        (
            PathBuf::from(r"C:\Games\Game"),
            PathBuf::from(r"C:\users\steamuser\Docs"),
        )
    };

    let mut session = session_in(session_dir.path());
    session.declare_root(0, &game_loc);
    session.declare_root(1, &docs_loc);
    let mut root0 = RootSources::new();
    add_source(
        &session,
        &mut root0,
        0,
        "/",
        0,
        Arc::new(DiskProvider::new(game.path())),
    );
    let mut root1 = RootSources::new();
    add_source(
        &session,
        &mut root1,
        1,
        "/",
        0,
        Arc::new(DiskProvider::new(docs.path())),
    );

    let declared = session.declared_roots();
    assert_eq!(
        declared.len(),
        1,
        "`declared_roots` lists the roots beyond root 0, so exactly root 1 — \
         root 0's declared path is its location, asserted below: {declared:?}"
    );
    assert_eq!(declared[0].0, 1);
    assert_eq!(
        declared[0].1, docs_loc,
        "root 1's declared location is not the one the config named"
    );
    // Both providers are mounted too — declaring must not have
    // replaced mounting, only joined it.
    let kernel = session.kernel();
    let read_root = |root: u32| -> Vec<u8> {
        let mut buf = [0u8; 8];
        let (fh, _, _) = kernel.open(RootId(root), "a.txt", OPEN_READ).unwrap();
        let n = kernel.read(fh, 0, &mut buf).unwrap();
        kernel.close(fh).unwrap();
        buf[..n].to_vec()
    };
    assert_eq!(read_root(0), b"g");
    assert_eq!(read_root(1), b"d");

    // Root 0 was declared as well: its declared path replaces the session's
    // default, and is what the session reports as root 0's location.
    let root0_location = session
        .root_locations()
        .into_iter()
        .find(|r| r.id == 0)
        .expect("root 0 always has a location")
        .location;
    assert_eq!(
        PathBuf::from(root0_location),
        game_loc,
        "root 0's declared location must reach the live session as its root"
    );
}
