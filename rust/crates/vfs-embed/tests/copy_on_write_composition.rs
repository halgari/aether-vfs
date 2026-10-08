//! **Copy-on-write over read-only layered content**, in the mount shape a
//! live session actually builds.
//!
//! The shim's write fall-through is sealed: a write the director will not serve
//! fails instead of quietly landing in a shim-local overlay. So a composition
//! that cannot copy up is a live failure, and the suite has to compose the
//! production shape to see it.
//!
//! The failure mode: mounting a writable `overrides` directory as one more
//! *sibling* layer in the same `MountGraph` as the read-only zip. A
//! `MountGraph` can route a write to whichever mount will take it; it cannot
//! seed a destination from a lower layer first. So an in-place edit of zip
//! content — `fopen(..., "r+b")`, `CreateFile(OPEN_EXISTING, GENERIC_WRITE)`,
//! what every mod tool and every ini writer does — walked past the writable
//! mounts (they do not hold the file, and an edit carries no create
//! disposition), reached the zip, and was refused `ST_READ_ONLY`. Before the
//! fall-through closed, that same open fell through to the shim's overlay and
//! "worked". 526 tests stayed green through both states.
//!
//! The fix is composition, not routing: the writable layer is an
//! `OverlayProvider` **upper** over the whole read-only graph
//! ([`Session::set_write_layer`]), so the director itself copies up. These
//! tests build the same five layers `skyrim-live` builds — root disk, staging
//! disk, zip, mods disk, write layer — and drive `Director`, which is what
//! the ring's `OP_OPEN` calls.
//!
//! The second half composes the same archive and mod tree the way a host that
//! learns its sources one at a time does — [`RootSources`] and
//! `Session::set_root_mounts` — where a rebuild on every source must keep the
//! write layer composed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use vfs_embed::{DiskProvider, Provider, RootId, RootSources, Session, OPEN_READ, OPEN_WRITE};

/// The zip-only file every test here edits, spelled as a real archive spells
/// it (`Data/…`) while every lookup uses the folded vpath the shim sends.
const ZIP_ENTRY: &str = "Data/x.esp";
const ZIP_VPATH: &str = "data/x.esp";
const ORIGINAL: &[u8] = b"ORIGINAL-ESP-BYTES";

struct Layout {
    _base: PathBuf,
    root: PathBuf,
    staging: PathBuf,
    mods: PathBuf,
    overrides: PathBuf,
    zip: PathBuf,
}

fn layout(name: &str) -> Layout {
    let base = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("vfs-cow-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let l = Layout {
        root: base.join("root"),
        staging: base.join("stage"),
        mods: base.join("mods"),
        // `overrides/root-0` — the root-scoped subdirectory the shim's own
        // overlay uses and `skyrim-live` mounts (`Session::overlay_layer_dir`).
        overrides: base.join("overrides").join("root-0"),
        zip: base.join("content.zip"),
        _base: base,
    };
    for d in [&l.root, &l.staging, &l.mods, &l.overrides] {
        std::fs::create_dir_all(d).unwrap();
    }
    write_stored_zip(&l.zip, ZIP_ENTRY, ORIGINAL);
    l
}

/// The four **read** layers, bottom to top, in `skyrim-live`'s own order:
/// the managed root's own directory and the staging directory (lowest, so
/// real content always wins), then the game archive, then the mod tree.
fn mount_read_layers(s: &Session, l: &Layout) {
    s.mount("", Arc::new(DiskProvider::new(&l.root))).unwrap();
    s.mount("", Arc::new(DiskProvider::new(&l.staging)))
        .unwrap();
    s.mount(
        "",
        Arc::new(vfs_zip::ZipProvider::open(&l.zip).expect("zip index")),
    )
    .unwrap();
    s.mount("", Arc::new(DiskProvider::new(&l.mods))).unwrap();
}

fn read_whole(s: &Session, vpath: &str) -> Vec<u8> {
    let k = s.kernel();
    let (fh, size, _) = k
        .open(RootId::DEFAULT, vpath, OPEN_READ)
        .expect("open for read");
    let mut buf = vec![0u8; size as usize];
    let mut off = 0usize;
    while off < buf.len() {
        match k.read(fh, off as u64, &mut buf[off..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => off += n,
        }
    }
    k.close(fh).unwrap();
    buf.truncate(off);
    buf
}

/// The regression, stated as a test: an in-place edit of content only a
/// read-only layer holds must succeed, land in the writable layer, and leave
/// the read-only source alone.
#[test]
fn an_in_place_edit_of_read_only_layered_content_lands_in_the_write_layer() {
    let l = layout("inplace");
    let zip_before = std::fs::read(&l.zip).unwrap();

    let s = Session::new();
    mount_read_layers(&s, &l);
    s.set_write_layer(Arc::new(DiskProvider::new(&l.overrides)))
        .expect("the write layer must be accepted");

    let k = s.kernel();
    // Exactly what `fopen(path, "r+b")` becomes by the time it reaches the
    // ring: OPEN_WRITE with **no** create/truncate bits. Nothing writable
    // holds this path, so only copy-up can answer it.
    let (fh, size, is_dir) = k.open(RootId::DEFAULT, ZIP_VPATH, OPEN_WRITE).expect(
        "an in-place edit of read-only layered content must be served by copy-up. \
             ST_READ_ONLY here is the regression this test exists for: the writable layer \
             is a sibling mount again instead of an overlay upper",
    );
    assert!(!is_dir);
    assert_eq!(
        size as usize,
        ORIGINAL.len(),
        "the handle must open onto the copied-up content, not an empty file — a zero size \
         means the write layer created a blank file instead of seeding from the archive"
    );
    // Overwrite in the middle and leave both ends alone: a truncating or
    // blank-file implementation cannot produce this result.
    assert_eq!(k.write(fh, 9, b"EDITED").unwrap(), 6);
    k.close(fh).unwrap();

    let mut expected = ORIGINAL.to_vec();
    expected[9..15].copy_from_slice(b"EDITED");

    assert_eq!(
        read_whole(&s, ZIP_VPATH),
        expected,
        "the edit must be visible through the director, with the untouched bytes preserved"
    );
    assert_eq!(
        std::fs::read(l.overrides.join("data").join("x.esp")).ok(),
        Some(expected.clone()),
        "the edited file must physically live in the write layer"
    );

    // The read-only source is untouched, byte for byte.
    assert_eq!(
        std::fs::read(&l.zip).unwrap(),
        zip_before,
        "copy-up mutated the archive it copied from"
    );
    // …and no other layer received a stray copy. A write that landed in the
    // managed root's own directory would be the escape the whole gate exists
    // to prevent.
    for (label, dir) in [
        ("root", &l.root),
        ("staging", &l.staging),
        ("mods", &l.mods),
    ] {
        assert!(
            !dir.join("data").join("x.esp").exists(),
            "the write leaked into the {label} layer at {dir:?}"
        );
    }

    // No `.cu.` staging file survives a successful copy-up.
    let strays: Vec<PathBuf> = std::fs::read_dir(l.overrides.join("data"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(".cu."))
        })
        .collect();
    assert!(
        strays.is_empty(),
        "copy-up left temp files behind: {strays:?}"
    );
}

/// The negative control for the test above, and the shape that regressed:
/// the identical layers with the writable directory mounted as a **sibling**
/// rather than as the overlay upper. The edit is refused.
///
/// This is here so the passing test above cannot be read as "writes work
/// anyway". The two differ in one call.
#[test]
fn the_same_layers_with_the_write_layer_mounted_as_a_sibling_cannot_edit_in_place() {
    let l = layout("sibling");

    let s = Session::new();
    mount_read_layers(&s, &l);
    // The pre-fix composition: one more sibling mount at the same prefix.
    s.mount("", Arc::new(DiskProvider::new(&l.overrides)))
        .unwrap();

    let err = s
        .kernel()
        .open(RootId::DEFAULT, ZIP_VPATH, OPEN_WRITE)
        .expect_err("a sibling writable mount cannot copy up, so this open cannot succeed");
    assert_eq!(
        err,
        vfs_provider::ST_READ_ONLY,
        "the archive owns this path and is read-only, so the graph refuses the write — the \
         exact failure the overlay composition removes"
    );

    // The control that keeps the assertion above honest: the same path still
    // reads fine through this graph, so the refusal is about writes.
    assert_eq!(read_whole(&s, ZIP_VPATH), ORIGINAL);
}

/// A brand-new file (create disposition present) must still work through the
/// overlay composition, and must still land in the write layer — the case
/// `scenario_toml_*_writepath` covers end to end, re-asserted here against
/// the composition those scenarios do not use.
#[test]
fn a_brand_new_file_still_lands_in_the_write_layer() {
    let l = layout("create");
    let s = Session::new();
    mount_read_layers(&s, &l);
    s.set_write_layer(Arc::new(DiskProvider::new(&l.overrides)))
        .unwrap();

    let k = s.kernel();
    let (fh, _, _) = k
        .open(
            RootId::DEFAULT,
            "data/brand-new.txt",
            OPEN_WRITE | vfs_protocol::OPEN_CREATE,
        )
        .expect("a create must be served by the write layer");
    k.write(fh, 0, b"NEW").unwrap();
    k.close(fh).unwrap();

    assert_eq!(read_whole(&s, "data/brand-new.txt"), b"NEW");
    assert_eq!(
        std::fs::read(l.overrides.join("data").join("brand-new.txt")).ok(),
        Some(b"NEW".to_vec())
    );
}

/// A read layer's content must still win over nothing, and the write layer
/// must not shadow it with an empty placeholder: reads of untouched archive
/// content are unchanged by the overlay composition.
#[test]
fn reads_of_untouched_layered_content_are_unchanged_by_the_write_layer() {
    let l = layout("reads");
    std::fs::write(l.mods.join("modfile.txt"), b"FROM-MODS").unwrap();

    let s = Session::new();
    mount_read_layers(&s, &l);
    s.set_write_layer(Arc::new(DiskProvider::new(&l.overrides)))
        .unwrap();

    assert_eq!(read_whole(&s, ZIP_VPATH), ORIGINAL);
    assert_eq!(read_whole(&s, "modfile.txt"), b"FROM-MODS");
    let names: Vec<String> = s
        .kernel()
        .readdir(RootId::DEFAULT, "data")
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(
        names.iter().any(|n| n.eq_ignore_ascii_case("x.esp")),
        "the archive's Data listing must survive the overlay composition, got {names:?}"
    );
}

/// A write open of a path the read layers serve as a **directory** must not
/// create a file over it in the write layer.
///
/// `CreateFileW(dir, GENERIC_WRITE, OPEN_ALWAYS, FILE_FLAG_BACKUP_SEMANTICS)`
/// sets no `FILE_DIRECTORY_FILE`, so nothing upstream recognises it as a
/// directory open; the disposition reaches the ring as `FILE_OPEN_IF`
/// carrying `OPEN_CREATE`, and a `DiskProvider` upper will happily create a
/// *file* named `data`. That file then shadows the directory — and, before
/// the companion fix below, made the entire subtree unlistable.
#[test]
fn a_write_open_of_a_layered_directory_does_not_create_a_file_over_it() {
    let l = layout("dircreate");
    let s = Session::new();
    mount_read_layers(&s, &l);
    s.set_write_layer(Arc::new(DiskProvider::new(&l.overrides)))
        .unwrap();

    // `data` exists only implicitly, as the parent of the zip's `Data/x.esp`
    // — the same way a real archive carries its directories.
    let st = s
        .kernel()
        .getattr(RootId::DEFAULT, "data")
        .unwrap()
        .unwrap();
    assert_eq!(
        st.kind,
        vfs_embed::KIND_DIR,
        "setup: `data` must resolve as a directory"
    );

    let err = s
        .kernel()
        .open(
            RootId::DEFAULT,
            "data",
            OPEN_WRITE | vfs_protocol::OPEN_CREATE,
        )
        .expect_err("a create over a layered directory must be refused, not honoured");
    assert_eq!(
        err,
        vfs_provider::ST_IS_DIR,
        "the refusal must say *why* — the shim turns ST_IS_DIR back into the directory open \
         the caller actually wanted, and any other status loses that"
    );
    assert!(
        !l.overrides.join("data").exists(),
        "a file named after the directory was created in the write layer; it shadows the \
         directory for every later lookup"
    );
    // The directory is still a directory afterwards.
    assert_eq!(
        s.kernel()
            .getattr(RootId::DEFAULT, "data")
            .unwrap()
            .unwrap()
            .kind,
        vfs_embed::KIND_DIR
    );
}

/// …and if such a file gets into the write layer by any other route, it must
/// cost the caller that one entry — not the whole directory's contents.
///
/// `OverlayProvider::readdir` must not propagate the upper's `not_a_dir`,
/// as `MountGraph` does not. For a game's `Data` directory that is the
/// difference between "one stray file is invisible" and "the game sees no
/// content at all", which is what turns a narrow bug into a broad one.
#[test]
fn a_stray_file_in_the_write_layer_does_not_break_a_directorys_listing() {
    let l = layout("straylisting");
    // Planted directly on disk, deliberately bypassing the provider graph:
    // the point is resilience to a write layer that is already in this state,
    // whatever produced it.
    std::fs::write(
        l.overrides.join("data"),
        b"a file where a directory belongs",
    )
    .unwrap();

    let s = Session::new();
    mount_read_layers(&s, &l);
    s.set_write_layer(Arc::new(DiskProvider::new(&l.overrides)))
        .unwrap();

    let names: Vec<String> = s
        .kernel()
        .readdir(RootId::DEFAULT, "data")
        .expect(
            "a stray file in the write layer must not fail the whole listing — this is the \
             difference between losing one entry and the game seeing no content at all",
        )
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(
        names.iter().any(|n| n.eq_ignore_ascii_case("x.esp")),
        "the read layers' entries must survive the stray upper file, got {names:?}"
    );
    // The content underneath is still readable.
    assert_eq!(read_whole(&s, ZIP_VPATH), ORIGINAL);
}

/// A path that is not a directory in *either* layer must still report itself
/// as one — the resilience above must not turn every mistake into an empty
/// listing.
#[test]
fn listing_a_plain_file_still_reports_not_a_directory() {
    let l = layout("notadir");
    let s = Session::new();
    mount_read_layers(&s, &l);
    s.set_write_layer(Arc::new(DiskProvider::new(&l.overrides)))
        .unwrap();

    let err = s
        .kernel()
        .readdir(RootId::DEFAULT, ZIP_VPATH)
        .expect_err("listing a file is not a listing");
    assert_eq!(err, vfs_provider::ST_NOT_A_DIRECTORY);
}

/// A write layer that is not writable is refused where it is declared, not at
/// the first write — the same fail-fast `OverlayProvider::new` applies, routed
/// through the `Session` API a host actually calls.
#[test]
fn a_read_only_write_layer_is_refused_at_declaration() {
    let l = layout("badupper");
    let s = Session::new();
    mount_read_layers(&s, &l);
    let err = s
        .set_write_layer(Arc::new(vfs_zip::ZipProvider::open(&l.zip).unwrap()))
        .expect_err("a read-only provider cannot be a write layer");
    assert_eq!(err, vfs_provider::ST_BAD_REQUEST);
}

// ── The incremental-host surface: `RootSources` + `set_root_mounts` ──────────
//
// Ported from the removed daemon's `copy_on_write_daemon.rs`. The tests above
// compose with `Session::mount`, as `skyrim-live` does. A host that learns its
// sources one at a time — the daemon was one, a config loader or a UI is
// another — records them in a `RootSources` and reinstalls the root's whole
// mount list with `Session::set_root_mounts` on every source, because
// `Director` holds one provider per root. That rebuild once composed the graph
// itself and mounted it on `Director`, bypassing the write layer entirely: a
// session with an archive plus a writable directory could not edit archive
// content in place — the write routed to the topmost writable *sibling*,
// which does not hold the file, and failed `ST_NOT_FOUND` (recorded before
// the fix, by the negative control below).
//
// These tests build that shape — sources added one at a time through
// `RootSources`, then a write layer — and drive `Director`, which is what the
// ring's `OP_OPEN` calls.

/// A modded game's directories, as an incremental host declares them: one
/// read-only archive, one mod tree, one place writes go.
struct SourcesLayout {
    _base: vfs_testkit::Scratch,
    zip: PathBuf,
    mods: PathBuf,
}

fn sources_layout() -> SourcesLayout {
    let base = vfs_testkit::scratch_dir("vfs-cow-sources");
    let zip = base.path().join("content.zip");
    write_stored_zip(&zip, ZIP_ENTRY, ORIGINAL);
    let mods = base.path().join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    SourcesLayout {
        _base: base,
        zip,
        mods,
    }
}

/// A session whose directories live in a scratch directory of its own, with
/// the per-root source lists it is composed from.
struct SourcedSession {
    session: Session,
    roots: std::collections::BTreeMap<u32, RootSources>,
    _dirs: vfs_testkit::Scratch,
}

impl SourcedSession {
    fn new() -> Self {
        let dirs = vfs_testkit::scratch_dir("vfs-cow-session");
        let mut session = Session::new();
        session.set_root(dirs.path().join("root"));
        session.set_overlay(dirs.path().join("overlay"));
        session.set_state_dir(dirs.path().join("state"));
        SourcedSession {
            session,
            roots: Default::default(),
            _dirs: dirs,
        }
    }

    /// Record one source and reinstall `root`'s whole mount list.
    fn add_source(
        &mut self,
        root: u32,
        mount: &str,
        layer: i32,
        provider: Arc<dyn Provider>,
    ) -> Result<(), i32> {
        let sources = self.roots.entry(root).or_default();
        sources.add(mount, layer, provider);
        let mounts = sources.mounts().expect("compose root");
        self.session.set_root_mounts(RootId(root), mounts)
    }

    /// The read sources, added the way a config's `[[source]]` list is
    /// added: archive first, mod tree above it.
    fn add_read_sources(&mut self, l: &SourcesLayout) {
        let zip = vfs_zip::ZipProvider::open(&l.zip).expect("zip source");
        self.add_source(0, "/", 0, Arc::new(zip)).unwrap();
        self.add_source(0, "/", 10, Arc::new(DiskProvider::new(&l.mods)))
            .unwrap();
    }

    /// Where this session's writes land: the root-scoped subdirectory of the
    /// session's own overlay, which is the same physical location the injected
    /// shim's overlay uses (see `Session::overlay_layer_dir`) — so host and
    /// shim agree on one directory for root 0's writes.
    fn write_layer_dir(&self) -> PathBuf {
        let dir = self.session.overlay_layer_dir(RootId::DEFAULT);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_for_in_place_edit(&self) -> Result<(u64, u64), i32> {
        // Exactly what `fopen(path, "r+b")` becomes by the time it reaches
        // the ring: OPEN_WRITE with **no** create/truncate bits. Nothing
        // writable holds this path, so only copy-up can answer it.
        self.session
            .kernel()
            .open(RootId::DEFAULT, ZIP_VPATH, OPEN_WRITE)
            .map(|(fh, size, is_dir)| {
                assert!(!is_dir);
                (fh, size)
            })
    }
}

/// The headline: a session built the way an incremental host builds one
/// edits content only the archive holds — and the archive is untouched
/// afterwards.
#[test]
fn an_in_place_edit_of_archive_content_copies_up_on_the_daemon_surface() {
    let l = sources_layout();
    let zip_before = std::fs::read(&l.zip).unwrap();

    let mut s = SourcedSession::new();
    s.add_read_sources(&l);
    let overrides = s.write_layer_dir();
    s.session
        .set_write_layer_at(RootId::DEFAULT, Arc::new(DiskProvider::new(&overrides)))
        .expect("the write layer must be accepted");

    let (fh, size) = s.open_for_in_place_edit().expect(
        "an in-place edit of archive content must be served by copy-up on the incremental \
         surface. ST_NOT_FOUND here is the gap this test exists for: the rebuild composed \
         the graph itself and mounted it on `Director`, so the write layer was never part \
         of the composition",
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
    let k = s.session.kernel();
    assert_eq!(k.write(fh, 9, b"EDITED").unwrap(), 6);
    k.close(fh).unwrap();
    assert_eq!(
        s.session.read_file(ZIP_VPATH).unwrap(),
        expected,
        "the edit must be visible through the director, with the untouched bytes preserved"
    );

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
/// one more source instead of as the write layer. That is the only thing the
/// incremental surface could express before the write layer existed, and it
/// cannot copy up: the layered stack routes the write to its topmost
/// `ReadWrite` child, which does not hold the file.
///
/// Kept so the test above cannot be read as "writes work anyway".
#[test]
fn the_writable_directory_added_as_an_ordinary_source_cannot_edit_in_place() {
    let l = sources_layout();
    let mut s = SourcedSession::new();
    s.add_read_sources(&l);
    let overrides = s.write_layer_dir();
    s.add_source(0, "/", 20, Arc::new(DiskProvider::new(&overrides)))
        .unwrap();

    let err = s
        .open_for_in_place_edit()
        .expect_err("a sibling writable source cannot copy up, so this open cannot succeed");
    assert_eq!(
        err,
        vfs_provider::ST_NOT_FOUND,
        "the layered stack sends the write to the topmost writable source, which does not \
         hold the file — the exact failure the write-layer composition removes"
    );

    // The control that keeps the assertion above honest: the same path still
    // reads fine through this session, so the refusal is about writes.
    assert_eq!(s.session.read_file(ZIP_VPATH).unwrap(), ORIGINAL);
}

/// The trap: a host reinstalls a root's whole mount list on every source. A
/// rebuild that composed the graph itself would **clobber** a write layer set
/// earlier — leaving a session that had copy-on-write until the next source
/// arrived. Sources are added in config order, so any config declaring its
/// write layer before its last source would silently lose it.
#[test]
fn a_source_added_after_the_write_layer_does_not_clobber_it() {
    let l = sources_layout();
    let mut s = SourcedSession::new();

    let overrides = s.write_layer_dir();
    s.session
        .set_write_layer_at(RootId::DEFAULT, Arc::new(DiskProvider::new(&overrides)))
        .unwrap();
    // Both sources arrive *after* the write layer, each triggering a rebuild.
    s.add_read_sources(&l);

    let (fh, size) = s
        .open_for_in_place_edit()
        .expect("the write layer set before the sources must survive their rebuilds");
    assert_eq!(size as usize, ORIGINAL.len());
    let k = s.session.kernel();
    k.write(fh, 9, b"EDITED").unwrap();
    k.close(fh).unwrap();
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
    let l = sources_layout();
    let mut s = SourcedSession::new();
    s.add_read_sources(&l);
    let overrides = s.write_layer_dir();
    s.session
        .set_write_layer_at(RootId(1), Arc::new(DiskProvider::new(&overrides)))
        .unwrap();

    // Root 0 must still be root 0: composing a *second* root must not
    // republish itself over the first, which would take the archive away
    // from every reader as well as leaving the write unanswered.
    assert_eq!(
        s.session.read_file(ZIP_VPATH).unwrap(),
        ORIGINAL,
        "root 0's own sources must survive another root being composed"
    );

    let err = s
        .open_for_in_place_edit()
        .expect_err("root 1's write layer must not answer for root 0");
    assert_eq!(
        err,
        vfs_provider::ST_NOT_FOUND,
        "root 0 is composed without a write layer, so it fails exactly as it did before \
         the write layer existed — the layered stack routes the write to the writable mod \
         source, which does not hold the file"
    );
}

/// A read-only provider is refused **where it is declared**, not at the first
/// write — a session that accepted an unwritable write layer would look
/// configured and fail hours later, on the first in-place edit.
#[test]
fn a_read_only_write_layer_is_refused_by_the_registry() {
    let l = sources_layout();
    let mut s = SourcedSession::new();
    s.add_read_sources(&l);
    let zip = vfs_zip::ZipProvider::open(&l.zip).unwrap();
    let err = s
        .session
        .set_write_layer_at(RootId::DEFAULT, Arc::new(zip))
        .expect_err("a read-only provider cannot be a write layer");
    assert_eq!(
        err,
        vfs_provider::ST_BAD_REQUEST,
        "expected a bad-request status"
    );

    // …and the session is left exactly as it was, not holding a refused layer
    // that would poison the next rebuild: adding another source still
    // succeeds, and reads still work.
    s.add_source(0, "/", 20, Arc::new(DiskProvider::new(&l.mods)))
        .expect("a refused write layer must not break later composition");
    assert_eq!(s.session.read_file(ZIP_VPATH).unwrap(), ORIGINAL);
}

/// A write layer whose directory does not exist yet still gives the session
/// copy-on-write: a user's overwrite folder need not exist before the first
/// edit, and copy-up has to make it rather than failing.
///
/// The core half of the daemon's
/// `a_write_layer_declared_over_grpc_gives_the_session_copy_on_write`, which
/// declared the layer over the wire; the wire's own validation (a sub-path
/// write layer, an unknown session) went with the daemon — `Session` has no
/// way to express either.
#[test]
fn a_write_layer_whose_directory_does_not_exist_yet_gives_copy_on_write() {
    let l = sources_layout();
    let zip_before = std::fs::read(&l.zip).unwrap();

    let mut s = SourcedSession::new();
    let zip = vfs_zip::ZipProvider::open(&l.zip).expect("zip source");
    s.add_source(0, "/", 0, Arc::new(zip)).unwrap();

    // Deliberately **not** created here.
    let overwrite_parent = vfs_testkit::tempdir().unwrap();
    let overrides = overwrite_parent.path().join("declared-overwrite");
    s.session
        .set_write_layer_at(RootId::DEFAULT, Arc::new(DiskProvider::new(&overrides)))
        .expect("the write layer must be accepted");

    let mut expected = ORIGINAL.to_vec();
    expected[9..15].copy_from_slice(b"EDITED");
    let (fh, size) = s
        .open_for_in_place_edit()
        .expect("a write layer whose directory does not exist yet must still give copy-up");
    assert_eq!(size as usize, ORIGINAL.len());
    let k = s.session.kernel();
    k.write(fh, 9, b"EDITED").unwrap();
    k.close(fh).unwrap();
    assert_eq!(s.session.read_file(ZIP_VPATH).unwrap(), expected);
    assert_eq!(
        std::fs::read(overrides.join("data").join("x.esp")).ok(),
        Some(expected),
        "the edit must land in the directory the host named"
    );
    assert_eq!(
        std::fs::read(&l.zip).unwrap(),
        zip_before,
        "copy-up mutated the archive it copied from"
    );
}

// ── a one-entry Stored zip, as `unicode_case_fold_across_the_ring` writes one ──

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
    std::fs::File::create(path)
        .unwrap()
        .write_all(&buf)
        .unwrap();
}
