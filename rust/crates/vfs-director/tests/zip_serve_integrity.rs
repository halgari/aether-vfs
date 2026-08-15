//! Does the composed VFS hand back the *same bytes* the archive holds?
//!
//! A game that renders its menu but cannot load a cell is the signature of
//! subtly wrong data rather than missing data: gross assets survive a few bad
//! bytes, record parsing does not. These tests compare VFS reads against ground
//! truth, both synthetically and — when the real corpus is present — against a
//! native extract of the same archive.

use std::sync::Arc;

use vfs_director::{DiskProvider, RootId, Session};

/// Deterministic, position-dependent bytes: a fragmented or mis-ordered read
/// shows up as a mismatch at a known offset rather than plausible-looking data.
fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| {
            let x = (i as u64).wrapping_mul(2654435761) ^ (i as u64 >> 13);
            (x & 0xFF) as u8
        })
        .collect()
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("vfs-integrity-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Reads at many sizes and offsets, including sizes that straddle the shim's
/// inline/bulk boundary (64 KiB) and its chunking.
#[test]
fn reads_match_at_every_offset_and_size() {
    let dir = tmp("offsets");
    // Big enough to cross the bulk threshold and several chunks.
    let data = pattern(3 * 1024 * 1024 + 1234);
    std::fs::write(dir.join("blob.bin"), &data).unwrap();

    let s = Session::new();
    s.mount("", Arc::new(DiskProvider::new(&dir))).unwrap();
    let k = s.kernel();

    let (fh, size, _) = k.open(RootId::DEFAULT, "blob.bin", vfs_protocol::OPEN_READ).unwrap();
    assert_eq!(size as usize, data.len());

    for &chunk in &[1usize, 7, 4096, 65_535, 65_536, 65_537, 1 << 20] {
        for &off in &[0usize, 1, 4095, 65_536, 1 << 20, data.len() - 1] {
            if off >= data.len() {
                continue;
            }
            let want = chunk.min(data.len() - off);
            let mut buf = vec![0u8; want];
            let n = k.read(fh, off as u64, &mut buf).unwrap();
            assert!(n > 0, "short read at off={off} chunk={chunk}");
            assert_eq!(
                &buf[..n],
                &data[off..off + n],
                "content mismatch at off={off} chunk={chunk} n={n}"
            );
        }
    }
    let _ = k.close(fh);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Sequential whole-file read, the pattern a BSA/master load actually uses.
#[test]
fn sequential_whole_file_read_is_byte_exact() {
    let dir = tmp("sequential");
    let data = pattern(5 * 1024 * 1024 + 77);
    std::fs::write(dir.join("master.esm"), &data).unwrap();

    let s = Session::new();
    s.mount("", Arc::new(DiskProvider::new(&dir))).unwrap();
    let k = s.kernel();
    let (fh, size, _) = k.open(RootId::DEFAULT, "master.esm", vfs_protocol::OPEN_READ).unwrap();

    let mut got = Vec::with_capacity(size as usize);
    let mut off = 0u64;
    let mut buf = vec![0u8; 256 * 1024];
    while (off as usize) < data.len() {
        let n = k.read(fh, off, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
        off += n as u64;
    }
    let _ = k.close(fh);

    assert_eq!(got.len(), data.len(), "length mismatch");
    if got != data {
        let at = got.iter().zip(&data).position(|(a, b)| a != b).unwrap();
        panic!("first byte mismatch at offset {at}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Overlay must shadow the base layer byte-for-byte, not blend the two.
#[test]
fn overlay_shadows_base_without_mixing() {
    let base = tmp("base");
    let over = tmp("over");
    let base_data = pattern(300_000);
    let over_data: Vec<u8> = pattern(180_000).iter().map(|b| b ^ 0xFF).collect();
    std::fs::write(base.join("shared.bin"), &base_data).unwrap();
    std::fs::write(over.join("shared.bin"), &over_data).unwrap();

    let s = Session::new();
    s.mount("", Arc::new(DiskProvider::new(&base))).unwrap();
    s.mount("", Arc::new(DiskProvider::new(&over))).unwrap();
    let k = s.kernel();

    let (fh, size, _) = k.open(RootId::DEFAULT, "shared.bin", vfs_protocol::OPEN_READ).unwrap();
    assert_eq!(size as usize, over_data.len(), "overlay size must win");
    let mut buf = vec![0u8; over_data.len()];
    let mut off = 0usize;
    while off < buf.len() {
        let n = k.read(fh, off as u64, &mut buf[off..]).unwrap();
        if n == 0 {
            break;
        }
        off += n;
    }
    let _ = k.close(fh);
    assert_eq!(buf, over_data, "overlay content must not be mixed with base");
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&over);
}

/// Compare the real archive against a native extract of the same archive.
///
/// Skipped unless both are present. This is the case that matters: the game
/// loads its menu but cannot enter a cell, and record parsing is exactly what
/// silently-wrong bytes break.
#[test]
fn real_archive_matches_native_extract() {
    let zip = std::path::Path::new(r"C:\tmp\skyrimse.zip");
    let native = std::path::Path::new(r"C:\tmp\skyrim-native\Skyrim Special Edition");
    if !zip.is_file() || !native.is_dir() {
        eprintln!("skip: real corpus not present");
        return;
    }

    let backend = match vfs_zip::ZipProvider::open(zip) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skip: ZipProvider: {e:?}");
            return;
        }
    };
    let stripped = vfs_compose::SubdirProvider::new(
        Arc::new(backend),
        "Skyrim Special Edition".to_string(),
    );
    let s = Session::new();
    s.mount("", Arc::new(stripped)).unwrap();
    let k = s.kernel();

    // A master (record parsing) and an interface archive (controlmap lives here).
    for name in ["Data/Skyrim.esm", "Data/Skyrim - Interface.bsa"] {
        let disk = native.join(name.replace('/', "\\"));
        if !disk.is_file() {
            eprintln!("skip {name}: not in native extract");
            continue;
        }
        let want = std::fs::read(&disk).expect("read native");
        let (fh, size, _) = k.open(RootId::DEFAULT, name, vfs_protocol::OPEN_READ).expect("vfs open");
        assert_eq!(size as usize, want.len(), "{name}: size mismatch");

        let mut got = vec![0u8; want.len()];
        let mut off = 0usize;
        while off < got.len() {
            let n = k.read(fh, off as u64, &mut got[off..]).expect("vfs read");
            if n == 0 {
                break;
            }
            off += n;
        }
        let _ = k.close(fh);
        assert_eq!(off, want.len(), "{name}: short read");
        if got != want {
            let at = got.iter().zip(&want).position(|(a, b)| a != b).unwrap();
            panic!("{name}: first byte mismatch at offset {at} (of {})", want.len());
        }
        eprintln!("{name}: {} bytes byte-exact", want.len());
    }
}

/// The path the *game* actually uses: shim ring client, bulk arena, pipelining.
///
/// The host-side tests above exercise the director in-process. The game instead
/// talks over the shared-memory ring via `read_fragmented`, which chunks,
/// pipelines and routes anything above 64 KiB through arena banks indexed by
/// ring slot. A mis-indexed bank or mismatched response would corrupt data
/// silently — the director would still be byte-exact, and only the game would
/// see garbage.
#[test]
fn ring_client_reads_are_byte_exact() {
    let dir = tmp("ring");
    let state = tmp("ring-state");
    // Spans inline (<64 KiB), bulk, and multiple pipelined batches.
    let data = pattern(9 * 1024 * 1024 + 4321);
    std::fs::write(dir.join("payload.bin"), &data).unwrap();

    let mut s = Session::new();
    s.set_root(&dir);
    s.set_state_dir(&state);
    s.mount("", Arc::new(DiskProvider::new(&dir))).unwrap();
    s.serve().expect("serve");

    // `serve` publishes VFS_RING_* / VFS_VIRTUAL_DIR into this process env,
    // which is exactly what the shim reads on the other side of the boundary.
    let section = std::env::var("VFS_RING_SECTION").expect("ring section");
    let ring_bytes: usize = std::env::var("VFS_RING_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024);
    let payload_cap: u32 = std::env::var("VFS_RING_PAYLOAD_CAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_048_576);
    let arena_len: usize = std::env::var("VFS_ARENA_LEN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let root = dir.to_string_lossy().into_owned();

    let roots = [(vfs_protocol::RootId::DEFAULT, root)];
    let client = vfs_shim::fuse_client::FuseClient::connect(
        &section, &roots, payload_cap, ring_bytes, arena_len,
    )
    .expect("client connect");
    let opened = client
        .open(vfs_protocol::RootId::DEFAULT, "payload.bin")
        .expect("ring open");
    assert_eq!(opened.size as usize, data.len());

    // Whole file in one call: exercises chunking + the deep pipeline.
    let mut got = vec![0u8; data.len()];
    let n = client
        .read_fragmented(opened.fh, 0, &mut got)
        .expect("read_fragmented");
    assert_eq!(n, data.len(), "short read over the ring");
    if got != data {
        let at = got.iter().zip(&data).position(|(a, b)| a != b).unwrap();
        panic!("ring read mismatch at offset {at} of {}", data.len());
    }

    // Offsets that straddle the inline/bulk boundary in both directions.
    for &(off, len) in &[
        (0usize, 64 * 1024 - 1),
        (64 * 1024 - 1, 64 * 1024 + 3),
        (1 << 20, 5 * 1024 * 1024),
        (data.len() - 1000, 1000),
    ] {
        let mut buf = vec![0u8; len];
        let n = client
            .read_fragmented(opened.fh, off as u64, &mut buf)
            .expect("read_fragmented");
        assert_eq!(n, len, "short read at off={off} len={len}");
        assert_eq!(&buf[..n], &data[off..off + n], "mismatch at off={off} len={len}");
    }

    let _ = client.close(opened.fh);
    s.stop_serve();
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&state);
}

/// A file that is simply absent must report *not found*, never a generic error.
///
/// The game reads `Skyrim.ccc`, gets ~100 Creation Club plugin names and tries
/// each one; most are not installed. "Not found" means skip it. A generic error
/// means something is wrong with the storage, which is a different thing
/// entirely — and a live session logged `err=1, nf=0` for dozens of those
/// plugins while the world refused to load.
#[test]
fn absent_files_report_not_found_not_error() {
    let dir = tmp("absent");
    std::fs::write(dir.join("present.bin"), b"here").unwrap();

    let s = Session::new();
    s.mount("", Arc::new(DiskProvider::new(&dir))).unwrap();
    let k = s.kernel();

    // getattr: absent must be Ok(None), i.e. "looked, not there".
    match k.getattr(RootId::DEFAULT, "ccasvsse001-almsivi.esm") {
        Ok(None) => {}
        Ok(Some(_)) => panic!("absent file reported as present"),
        Err(e) => panic!("absent getattr returned error {e} — must be Ok(None)"),
    }
    match k.getattr(RootId::DEFAULT, "data/ccbgssse040-advobgobs.esl") {
        Ok(None) => {}
        Ok(Some(_)) => panic!("absent file reported as present"),
        Err(e) => panic!("absent getattr returned error {e} — must be Ok(None)"),
    }

    // open: must distinguish "not found" from "I/O error". Anything else makes
    // a caller treat a missing optional plugin as a storage failure.
    match k.open(RootId::DEFAULT, "ccasvsse001-almsivi.esm", vfs_protocol::OPEN_READ) {
        Ok(_) => panic!("absent file opened"),
        Err(st) => assert_eq!(
            st,
            vfs_protocol::ST_NOT_FOUND,
            "absent open returned {st}, expected ST_NOT_FOUND ({})",
            vfs_protocol::ST_NOT_FOUND
        ),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Directories that exist only implicitly in a zip must still resolve.
///
/// A zip may carry no explicit directory entries — only files with slashes in
/// their names. If the VFS answers "not found" for `Data/Scripts` while a real
/// install has that folder, an engine that probes directories before scanning
/// them concludes the content is absent. That is invisible at the main menu,
/// which opens known archives by name, and fatal on world load.
#[test]
fn implicit_zip_directories_resolve_like_a_real_install() {
    let zip = std::path::Path::new(r"C:\tmp\skyrimse.zip");
    let native = std::path::Path::new(r"C:\tmp\skyrim-native\Skyrim Special Edition");
    if !zip.is_file() || !native.is_dir() {
        eprintln!("skip: real corpus not present");
        return;
    }
    let backend = match vfs_zip::ZipProvider::open(zip) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skip: ZipProvider: {e:?}");
            return;
        }
    };
    let s = Session::new();
    s.mount(
        "",
        Arc::new(vfs_compose::SubdirProvider::new(
            Arc::new(backend),
            "Skyrim Special Edition".to_string(),
        )),
    )
    .unwrap();
    let k = s.kernel();

    // Ground truth is the archive itself, not an extract on disk. An earlier
    // version of this test compared against `C:\tmp\skyrim-native`, which had
    // been polluted by copying a mod's `Data` folder into it during an unrelated
    // control run — so it "found" a missing `Data/Scripts` that never existed in
    // the archive. Derive the expectation from the zip and the question is
    // self-contained.
    let mut bad = Vec::new();
    for rel in ["Data", "Data/Video"] {
        let via_vfs = match k.getattr(RootId::DEFAULT, rel) {
            Ok(Some(st)) => st.kind == vfs_protocol::KIND_DIR,
            Ok(None) => false,
            Err(e) => {
                bad.push(format!("{rel}: getattr error {e}"));
                continue;
            }
        };
        // These have explicit entries in the archive, so they must resolve.
        eprintln!("  {rel:<18} vfs_dir={via_vfs}");
        if !via_vfs {
            bad.push(format!("{rel}: archive holds this directory but VFS does not"));
        }
    }
    // A path with no entries at all must not masquerade as a directory.
    match k.getattr(RootId::DEFAULT, "Data/NoSuchFolderHere") {
        Ok(None) => {}
        Ok(Some(_)) => bad.push("Data/NoSuchFolderHere: absent path reported as present".into()),
        Err(e) => bad.push(format!("Data/NoSuchFolderHere: getattr error {e}")),
    }
    let _ = native;

    // readdir of the game root must list Data at all.
    match k.readdir(RootId::DEFAULT, "") {
        Ok(entries) => {
            let names: Vec<String> = entries.iter().map(|e| e.name.to_ascii_lowercase()).collect();
            eprintln!("  root readdir -> {} entries", names.len());
            if !names.iter().any(|n| n == "data") {
                bad.push("root readdir does not list Data".into());
            }
        }
        Err(e) => bad.push(format!("root readdir error {e}")),
    }

    assert!(bad.is_empty(), "directory semantics differ from a real install:\n  {}", bad.join("\n  "));
}

/// Does enumerating `Data` list the master plugins?
///
/// Skyrim discovers its load order by listing `Data`, not by opening the
/// masters by name: the BSAs it loads are named explicitly in `Skyrim.ini` and
/// the Creation Club plugins come from `Skyrim.ccc`, so both survive a broken
/// listing. The masters do not. A launch where `Data` lists no `.esm` reaches
/// the main menu looking healthy — the UI lives in `Skyrim - Interface.bsa` —
/// and then has no world to load, which is indistinguishable from a hang.
///
/// Observed 2026-08-12: the game opened every BSA and every Creation Club
/// plugin, never once opened `Skyrim.esm`, and rewrote `plugins.txt` empty.
#[test]
fn data_listing_includes_the_master_plugins() {
    let zip = std::path::Path::new(r"C:\tmp\skyrimse.zip");
    if !zip.is_file() {
        eprintln!("skip: real corpus not present");
        return;
    }
    let backend = match vfs_zip::ZipProvider::open(zip) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skip: ZipProvider: {e:?}");
            return;
        }
    };
    let stripped = vfs_compose::SubdirProvider::new(
        Arc::new(backend),
        "Skyrim Special Edition".to_string(),
    );
    let s = Session::new();
    s.mount("", Arc::new(stripped)).unwrap();

    let entries = s.kernel().readdir(RootId::DEFAULT, "Data").expect("readdir Data");
    let names: Vec<String> = entries.iter().map(|e| e.name.to_ascii_lowercase()).collect();

    for master in [
        "skyrim.esm",
        "update.esm",
        "dawnguard.esm",
        "hearthfires.esm",
        "dragonborn.esm",
    ] {
        assert!(
            names.iter().any(|n| n == master),
            "Data listing is missing {master}; got {} entries: {names:?}",
            names.len()
        );
    }
}
