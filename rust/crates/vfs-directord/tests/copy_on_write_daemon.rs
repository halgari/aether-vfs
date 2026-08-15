//! **Copy-on-write on the daemon surface** — the one the product uses.
//!
//! Gate 4 Task 6 restored copy-on-write by composing a session's mount graph
//! as an `OverlayProvider` base with the writable layer as its upper, so the
//! director itself copies up. It proved that through `Session` directly,
//! which is what the `skyrim-live` **harness** builds. `SessionRegistry` —
//! the gRPC/TOML surface a real daemon session is built through — still
//! composed every source into one sibling stack and mounted it on
//! `Director` itself, bypassing that composition entirely. A daemon session
//! with an archive plus a writable directory therefore could not edit archive
//! content in place: the write routed to the topmost writable *sibling*,
//! which does not hold the file, and failed `ST_NOT_FOUND` (recorded before
//! the fix, by the negative control below).
//!
//! These tests build the daemon's own shape: `SessionRegistry::create` →
//! `add_source` per source → a write layer, then drive `Director`, which is
//! what the ring's `OP_OPEN` calls. The last one goes the whole way through
//! `AddSource` on the wire, since that — not the Rust API — is what a config
//! file reaches.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::TcpListener;
use tonic::transport::Server;
use vfs_control::pb::director_server::DirectorServer;
use vfs_control::pb::{source_spec, AddSourceReq, CreateSessionReq, DiskSource, ZipSource};
use vfs_control::SourceSpec;
use vfs_director::{DiskProvider, Provider, RootId, OPEN_WRITE};
use vfs_directord::{connect, DirectorService, SessionRegistry};
use vfs_source::build_provider;

/// The archive-only file every test here edits, spelled as a real archive
/// spells it (`Data/…`) while every lookup uses the folded vpath the shim
/// sends — matching `vfs-director`'s own copy-on-write tests.
const ZIP_ENTRY: &str = "Data/x.esp";
const ZIP_VPATH: &str = "data/x.esp";
const ORIGINAL: &[u8] = b"ORIGINAL-ESP-BYTES";

/// A modded game's directories, as a daemon session declares them: one
/// read-only archive, one mod tree, one place writes go.
struct Layout {
    _base: tempfile::TempDir,
    zip: PathBuf,
    mods: PathBuf,
}

fn layout() -> Layout {
    let base = tempfile::tempdir().expect("tempdir");
    let zip = base.path().join("content.zip");
    write_stored_zip(&zip, ZIP_ENTRY, ORIGINAL);
    let mods = base.path().join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    Layout {
        _base: base,
        zip,
        mods,
    }
}

/// The read sources, added exactly as `apply_session_config` adds a config's
/// `[[source]]` list: archive first, mod tree above it.
fn add_read_sources(reg: &SessionRegistry, session_id: &str, l: &Layout) {
    let zip = build_provider(&SourceSpec::Zip {
        path: l.zip.to_string_lossy().into_owned(),
    })
    .expect("zip source");
    reg.add_source(session_id, 0, "/", 0, zip).unwrap();
    let mods = build_provider(&SourceSpec::Disk {
        path: l.mods.to_string_lossy().into_owned(),
    })
    .expect("mods source");
    reg.add_source(session_id, 0, "/", 10, mods).unwrap();
}

/// Where this session's writes land: the root-scoped subdirectory of the
/// session's own overlay, which is the same physical location the injected
/// shim's overlay uses (see `Session::overlay_layer_dir`) — so host and shim
/// agree on one directory for root 0's writes.
fn write_layer_dir(reg: &SessionRegistry, session_id: &str) -> PathBuf {
    let dir = reg
        .with_session_mut(session_id, |live| {
            Ok(live.session.overlay_layer_dir(RootId::DEFAULT))
        })
        .unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn open_for_in_place_edit(reg: &SessionRegistry, session_id: &str) -> Result<(u64, u64), i32> {
    reg.with_session_mut(session_id, |live| {
        // Exactly what `fopen(path, "r+b")` becomes by the time it reaches
        // the ring: OPEN_WRITE with **no** create/truncate bits. Nothing
        // writable holds this path, so only copy-up can answer it.
        Ok(live
            .session
            .kernel()
            .open(RootId::DEFAULT, ZIP_VPATH, OPEN_WRITE)
            .map(|(fh, size, is_dir)| {
                assert!(!is_dir);
                (fh, size)
            }))
    })
    .unwrap()
}

/// The headline: a daemon session, built the way the daemon builds one, edits
/// content only the archive holds — and the archive is untouched afterwards.
#[test]
fn an_in_place_edit_of_archive_content_copies_up_on_the_daemon_surface() {
    let l = layout();
    let zip_before = std::fs::read(&l.zip).unwrap();

    let reg = SessionRegistry::new();
    let summary = reg.create("daemon-cow".into()).unwrap();
    add_read_sources(&reg, &summary.id, &l);
    let overrides = write_layer_dir(&reg, &summary.id);
    reg.set_write_layer(&summary.id, 0, Arc::new(DiskProvider::new(&overrides)))
        .expect("the write layer must be accepted");

    let (fh, size) = open_for_in_place_edit(&reg, &summary.id).expect(
        "an in-place edit of archive content must be served by copy-up on the daemon \
         surface. ST_NOT_FOUND here is the gap this test exists for: `add_source` \
         composed the graph itself and mounted it on `Director`, so the write layer \
         was never part of the composition",
    );
    assert_eq!(
        size as usize,
        ORIGINAL.len(),
        "the handle must open onto the copied-up content, not an empty file — a zero size \
         means the write layer created a blank file instead of seeding from the archive"
    );

    // Overwrite in the middle and leave both ends alone: a truncating or
    // blank-file implementation cannot produce this result.
    let mut expected = ORIGINAL.to_vec();
    expected[9..15].copy_from_slice(b"EDITED");
    reg.with_session_mut(&summary.id, |live| {
        let k = live.session.kernel();
        assert_eq!(k.write(fh, 9, b"EDITED").unwrap(), 6);
        k.close(fh).unwrap();
        assert_eq!(
            live.session.read_file(ZIP_VPATH).unwrap(),
            expected,
            "the edit must be visible through the director, with the untouched bytes preserved"
        );
        Ok(())
    })
    .unwrap();

    assert_eq!(
        std::fs::read(overrides.join("data").join("x.esp")).ok(),
        Some(expected),
        "the edited file must physically live in the write layer"
    );
    assert_eq!(
        std::fs::read(&l.zip).unwrap(),
        zip_before,
        "copy-up mutated the archive it copied from"
    );
    assert!(
        !l.mods.join("data").join("x.esp").exists(),
        "the write leaked into the mod tree at {:?}",
        l.mods
    );
}

/// The negative control, and the **pre-fix state recorded as a test**: the
/// same session, differing by one call — the writable directory arrives as
/// one more `add_source` instead of as the write layer. That is the only
/// thing the daemon surface could express before this task, and it cannot
/// copy up: the layered stack routes the write to its topmost `ReadWrite`
/// child, which does not hold the file.
///
/// Kept so the test above cannot be read as "writes work anyway".
#[test]
fn the_writable_directory_added_as_an_ordinary_source_cannot_edit_in_place() {
    let l = layout();
    let reg = SessionRegistry::new();
    let summary = reg.create("daemon-cow-sibling".into()).unwrap();
    add_read_sources(&reg, &summary.id, &l);
    let overrides = write_layer_dir(&reg, &summary.id);
    reg.add_source(&summary.id, 0, "/", 20, Arc::new(DiskProvider::new(&overrides)))
        .unwrap();

    let err = open_for_in_place_edit(&reg, &summary.id)
        .expect_err("a sibling writable source cannot copy up, so this open cannot succeed");
    assert_eq!(
        err,
        vfs_protocol::ST_NOT_FOUND,
        "the layered stack sends the write to the topmost writable source, which does not \
         hold the file — the exact failure the write-layer composition removes"
    );

    // The control that keeps the assertion above honest: the same path still
    // reads fine through this session, so the refusal is about writes.
    reg.with_session_mut(&summary.id, |live| {
        assert_eq!(live.session.read_file(ZIP_VPATH).unwrap(), ORIGINAL);
        Ok(())
    })
    .unwrap();
}

/// The trap this task was warned about: `add_source` rebuilds a root's whole
/// provider on every call. A rebuild that composed the graph itself would
/// **clobber** a write layer set earlier — leaving a session that had
/// copy-on-write until the next source arrived. Sources are added in config
/// order, so any config declaring its write layer before its last source
/// would silently lose it.
#[test]
fn a_source_added_after_the_write_layer_does_not_clobber_it() {
    let l = layout();
    let reg = SessionRegistry::new();
    let summary = reg.create("daemon-cow-order".into()).unwrap();

    let overrides = write_layer_dir(&reg, &summary.id);
    reg.set_write_layer(&summary.id, 0, Arc::new(DiskProvider::new(&overrides)))
        .unwrap();
    // Both sources arrive *after* the write layer, each triggering a rebuild.
    add_read_sources(&reg, &summary.id, &l);

    let (fh, size) = open_for_in_place_edit(&reg, &summary.id)
        .expect("the write layer set before the sources must survive their rebuilds");
    assert_eq!(size as usize, ORIGINAL.len());
    reg.with_session_mut(&summary.id, |live| {
        let k = live.session.kernel();
        k.write(fh, 9, b"EDITED").unwrap();
        k.close(fh).unwrap();
        Ok(())
    })
    .unwrap();
    let mut expected = ORIGINAL.to_vec();
    expected[9..15].copy_from_slice(b"EDITED");
    assert_eq!(
        std::fs::read(overrides.join("data").join("x.esp")).ok(),
        Some(expected)
    );
}

/// A write layer only some *other* root has must not give root 0 copy-up —
/// roots compose independently, and a session that silently shared one
/// writable directory across roots would put a second root's writes in the
/// game directory's overwrite folder.
#[test]
fn a_write_layer_on_another_root_does_not_serve_root_zero() {
    let l = layout();
    let reg = SessionRegistry::new();
    let summary = reg.create("daemon-cow-other-root".into()).unwrap();
    add_read_sources(&reg, &summary.id, &l);
    let overrides = write_layer_dir(&reg, &summary.id);
    reg.set_write_layer(&summary.id, 1, Arc::new(DiskProvider::new(&overrides)))
        .unwrap();

    // Root 0 must still be root 0: composing a *second* root must not
    // republish itself over the first, which would take the archive away
    // from every reader as well as leaving the write unanswered.
    reg.with_session_mut(&summary.id, |live| {
        assert_eq!(
            live.session.read_file(ZIP_VPATH).unwrap(),
            ORIGINAL,
            "root 0's own sources must survive another root being composed"
        );
        Ok(())
    })
    .unwrap();

    let err = open_for_in_place_edit(&reg, &summary.id)
        .expect_err("root 1's write layer must not answer for root 0");
    assert_eq!(
        err,
        vfs_protocol::ST_NOT_FOUND,
        "root 0 is composed without a write layer, so it fails exactly as it did before \
         this task — the layered stack routes the write to the writable mod source, which \
         does not hold the file"
    );
}

/// A read-only provider is refused **where it is declared**, not at the first
/// write — a session that accepted an unwritable write layer would look
/// configured and fail hours later, on the first in-place edit.
#[test]
fn a_read_only_write_layer_is_refused_by_the_registry() {
    let l = layout();
    let reg = SessionRegistry::new();
    let summary = reg.create("daemon-cow-badupper".into()).unwrap();
    add_read_sources(&reg, &summary.id, &l);
    let zip: Arc<dyn Provider> = build_provider(&SourceSpec::Zip {
        path: l.zip.to_string_lossy().into_owned(),
    })
    .unwrap();
    let err = reg
        .set_write_layer(&summary.id, 0, zip)
        .expect_err("a read-only provider cannot be a write layer");
    assert!(
        err.contains(&vfs_protocol::ST_BAD_REQUEST.to_string()),
        "expected a bad-request status in {err:?}"
    );

    // …and the session is left exactly as it was, not holding a refused layer
    // that would poison the next rebuild: adding another source still
    // succeeds, and reads still work.
    reg.add_source(
        &summary.id,
        0,
        "/",
        20,
        Arc::new(DiskProvider::new(&l.mods)),
    )
    .expect("a refused write layer must not break later composition");
    reg.with_session_mut(&summary.id, |live| {
        assert_eq!(live.session.read_file(ZIP_VPATH).unwrap(), ORIGINAL);
        Ok(())
    })
    .unwrap();
}

/// The whole way through the wire: `AddSourceReq { write_layer: true }`, the
/// field a config's `[[source]] write_layer = true` becomes in
/// `apply_session_config`. A Rust-only API would leave the actual product
/// surface — a TOML file handed to the daemon — still unable to ask for
/// copy-on-write.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_layer_declared_over_grpc_gives_the_session_copy_on_write() {
    let l = layout();
    let zip_before = std::fs::read(&l.zip).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let registry = SessionRegistry::new();
    let svc = DirectorService::new(registry.clone());
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let mut client = connect(&format!("{addr}")).await.unwrap();
    let session = client
        .create_session(CreateSessionReq {
            name: "wire-cow".into(),
        })
        .await
        .unwrap()
        .into_inner();

    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(vfs_control::pb::SourceSpec {
                kind: Some(source_spec::Kind::Zip(ZipSource {
                    path: l.zip.to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 0,
            root: 0,
            write_layer: false,
        })
        .await
        .expect("AddSource (archive)");

    // The overwrite directory a config would name. Deliberately **not**
    // created here: a user's overwrite folder need not exist yet, and copy-up
    // has to make it rather than failing on the first edit.
    let overrides = PathBuf::from(&session.root)
        .parent()
        .expect("session root has a parent")
        .join("declared-overwrite");
    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(vfs_control::pb::SourceSpec {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: overrides.to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 0,
            root: 0,
            write_layer: true,
        })
        .await
        .expect("AddSource (write layer)");

    let mut expected = ORIGINAL.to_vec();
    expected[9..15].copy_from_slice(b"EDITED");
    let (fh, size) = open_for_in_place_edit(&registry, &session.id)
        .expect("a write layer declared on the wire must give the session copy-up");
    assert_eq!(size as usize, ORIGINAL.len());
    registry
        .with_session_mut(&session.id, |live| {
            let k = live.session.kernel();
            k.write(fh, 9, b"EDITED").unwrap();
            k.close(fh).unwrap();
            assert_eq!(live.session.read_file(ZIP_VPATH).unwrap(), expected);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        std::fs::read(overrides.join("data").join("x.esp")).ok(),
        Some(expected),
        "the edit must land in the directory the wire named"
    );
    assert_eq!(
        std::fs::read(&l.zip).unwrap(),
        zip_before,
        "copy-up mutated the archive it copied from"
    );

    // A read-only source cannot be a write layer, and the wire says so at
    // declaration rather than accepting a session that cannot write.
    let err = client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(vfs_control::pb::SourceSpec {
                kind: Some(source_spec::Kind::Zip(ZipSource {
                    path: l.zip.to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 0,
            root: 0,
            write_layer: true,
        })
        .await
        .expect_err("a zip cannot be a write layer");
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    // Nor can a write layer be scoped to a sub-path: the upper covers the
    // whole root, so a prefix here would be accepted and then ignored.
    let err = client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(vfs_control::pb::SourceSpec {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: overrides.to_string_lossy().into_owned(),
                })),
            }),
            mount: "Data/SomeMod".into(),
            layer: 0,
            root: 0,
            write_layer: true,
        })
        .await
        .expect_err("a write layer cannot mount at a sub-path");
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
    assert!(err.message().contains("Data/SomeMod"), "{err:?}");

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .unwrap();
    server.abort();
}

/// The product surface end to end, minus the launch: a **TOML file** that
/// declares an archive and `write_layer = true`, driven through
/// `apply_session_config` — parse, validate, `AddSource` on the wire,
/// registry, session composition. Every link in that chain has to carry the
/// flag; a session that reads its config and quietly composes read-only
/// content is the shape this gate exists to rule out.
#[tokio::test(flavor = "multi_thread")]
async fn a_config_file_declaring_a_write_layer_gives_the_session_copy_on_write() {
    let l = layout();
    let zip_before = std::fs::read(&l.zip).unwrap();
    let overwrite = tempfile::tempdir().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let registry = SessionRegistry::new();
    let svc = DirectorService::new(registry.clone());
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let toml_text = format!(
        r#"
[session]
name = "cow-from-config"

[[source]]
type = "zip"
path = {}

[[source]]
type        = "disk"
path        = {}
write_layer = true
"#,
        toml_quote(&l.zip.to_string_lossy()),
        toml_quote(&overwrite.path().to_string_lossy()),
    );
    let cfg: vfs_control::SessionConfig = toml::from_str(&toml_text).expect("parse config");

    let mut client = connect(&format!("{addr}")).await.unwrap();
    let (session_id, exit) = vfs_directord::apply_session_config(&mut client, &cfg)
        .await
        .expect("apply config");
    assert_eq!(exit, None, "this config has no [launch] block");

    let (fh, size) = open_for_in_place_edit(&registry, &session_id)
        .expect("a config-declared write layer must give the session copy-up");
    assert_eq!(size as usize, ORIGINAL.len());
    let mut expected = ORIGINAL.to_vec();
    expected[9..15].copy_from_slice(b"EDITED");
    registry
        .with_session_mut(&session_id, |live| {
            let k = live.session.kernel();
            k.write(fh, 9, b"EDITED").unwrap();
            k.close(fh).unwrap();
            assert_eq!(live.session.read_file(ZIP_VPATH).unwrap(), expected);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        std::fs::read(overwrite.path().join("data").join("x.esp")).ok(),
        Some(expected),
        "the edit must land in the directory the config named"
    );
    assert_eq!(
        std::fs::read(&l.zip).unwrap(),
        zip_before,
        "copy-up mutated the archive it copied from"
    );

    client
        .teardown_session(vfs_control::pb::TeardownReq { session_id })
        .await
        .unwrap();
    server.abort();
}

fn toml_quote(s: &str) -> String {
    format!("{s:?}")
}

// ── a one-entry Stored zip, as `copy_on_write_composition.rs` writes one ──

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn write_stored_zip(path: &Path, entry: &str, content: &[u8]) {
    let mut buf = Vec::new();
    let crc = crc32(content);
    let n = entry.len() as u16;
    buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&n.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(entry.as_bytes());
    buf.extend_from_slice(content);
    let cd_start = buf.len() as u32;
    buf.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 6]);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&n.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&[0u8; 8]);
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(entry.as_bytes());
    let cd_size = buf.len() as u32 - cd_start;
    buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&cd_size.to_le_bytes());
    buf.extend_from_slice(&cd_start.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    std::fs::File::create(path).unwrap().write_all(&buf).unwrap();
}
