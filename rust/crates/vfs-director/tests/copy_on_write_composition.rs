//! **Copy-on-write over read-only layered content**, in the mount shape a
//! live session actually builds.
//!
//! Gate 4 Task 5 sealed the shim's write fall-through: a write the director
//! will not serve now fails instead of quietly landing in a shim-local
//! overlay. That exposed a regression nothing in the suite covered, because
//! nothing in the suite composed the production shape.
//!
//! `skyrim-live` mounted its writable `overrides` directory as one more
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

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use vfs_director::{DiskProvider, RootId, Session, OPEN_READ, OPEN_WRITE};

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
    let base = std::env::temp_dir().join(format!("vfs-cow-{}-{name}", std::process::id()));
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
    s.mount("", Arc::new(DiskProvider::new(&l.staging))).unwrap();
    s.mount(
        "",
        Arc::new(vfs_zip::ZipProvider::open(&l.zip).expect("zip index")),
    )
    .unwrap();
    s.mount("", Arc::new(DiskProvider::new(&l.mods))).unwrap();
}

fn read_whole(s: &Session, vpath: &str) -> Vec<u8> {
    let k = s.kernel();
    let (fh, size, _) = k.open(RootId::DEFAULT, vpath, OPEN_READ).expect("open for read");
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
    let (fh, size, is_dir) = k
        .open(RootId::DEFAULT, ZIP_VPATH, OPEN_WRITE)
        .expect(
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
    for (label, dir) in [("root", &l.root), ("staging", &l.staging), ("mods", &l.mods)] {
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
    assert!(strays.is_empty(), "copy-up left temp files behind: {strays:?}");
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
    s.mount("", Arc::new(DiskProvider::new(&l.overrides))).unwrap();

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
    s.set_write_layer(Arc::new(DiskProvider::new(&l.overrides))).unwrap();

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
    s.set_write_layer(Arc::new(DiskProvider::new(&l.overrides))).unwrap();

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
    std::fs::File::create(path).unwrap().write_all(&buf).unwrap();
}
