use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use aether_archive::extract::{ArchiveKind, extract, sniff};
use aether_archive::{FormatError, RangeRead};
use sevenz_rust2::{ArchiveEntry, ArchiveWriter, SourceReader};

/// Compressible, but not trivially (so frames have distinct sizes).
fn data(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|i| {
            if i % 7 == 0 {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (x >> 56) as u8
            } else {
                (i % 251) as u8
            }
        })
        .collect()
}

fn files() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("readme.txt", b"hello".to_vec()),
        ("Data/Meshes/a.nif", data(100_000, 21)),
        ("Data/b.esp", data(5_000, 22)),
        ("Data/empty.txt", Vec::new()),
    ]
}

/// One solid block holding every non-empty file.
fn solid_7z() -> Vec<u8> {
    let mut w = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    let (full, empty): (Vec<_>, Vec<_>) = files().into_iter().partition(|(_, d)| !d.is_empty());
    let entries = full
        .iter()
        .map(|(n, _)| ArchiveEntry::new_file(n))
        .collect();
    let readers = full
        .iter()
        .map(|(_, d)| SourceReader::new(Cursor::new(d.clone())))
        .collect();
    w.push_archive_entries(entries, readers).unwrap();
    for (n, _) in empty {
        w.push_archive_entry::<&[u8]>(ArchiveEntry::new_file(n), None)
            .unwrap();
    }
    w.push_archive_entry::<&[u8]>(ArchiveEntry::new_directory("Data"), None)
        .unwrap();
    w.finish().unwrap().into_inner()
}

/// One block per file; `big` is incompressible and comes first.
fn non_solid_7z(big: &[u8]) -> Vec<u8> {
    let mut w = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    w.push_archive_entry(ArchiveEntry::new_file("big.bin"), Some(big))
        .unwrap();
    w.push_archive_entry(ArchiveEntry::new_file("small.txt"), Some(&b"small"[..]))
        .unwrap();
    w.finish().unwrap().into_inner()
}

fn zip_file() -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    w.add_directory("Data/", opts).unwrap();
    for (n, d) in files() {
        w.start_file(n, opts).unwrap();
        w.write_all(&d).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn run(archive: Vec<u8>, wanted: &[&str]) -> Result<BTreeMap<String, Vec<u8>>, FormatError> {
    let wanted: Vec<String> = wanted.iter().map(|s| s.to_string()).collect();
    let mut got = BTreeMap::new();
    extract::<_, _, FormatError>(archive, &wanted, |path, r| {
        let mut v = Vec::new();
        r.read_to_end(&mut v)?;
        got.insert(path.to_string(), v);
        Ok(())
    })?;
    Ok(got)
}

#[test]
fn extracts_selected_entries_from_7z_and_zip() {
    let all = files();
    for archive in [solid_7z(), zip_file()] {
        let got = run(
            archive,
            &[r"DATA\MESHES\A.NIF", r"Data\empty.txt", "readme.txt"],
        )
        .unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[r"DATA\MESHES\A.NIF"], all[1].1);
        assert!(got[r"Data\empty.txt"].is_empty());
        assert_eq!(got["readme.txt"], b"hello");
    }
}

#[test]
fn callbacks_may_stop_reading_early() {
    let wanted = vec!["Data/Meshes/a.nif".to_string(), "Data/b.esp".to_string()];
    let mut seen = Vec::new();
    extract::<_, _, FormatError>(solid_7z(), &wanted, |path, r| {
        let mut four = [0u8; 4];
        r.read_exact(&mut four)?;
        seen.push((path.to_string(), four));
        Ok(())
    })
    .unwrap();
    assert_eq!(seen.len(), 2);
    let esp = &files()[2].1;
    assert!(
        seen.iter()
            .any(|(p, f)| p == "Data/b.esp" && f[..] == esp[..4])
    );
}

#[test]
fn missing_entries_are_reported() {
    for archive in [solid_7z(), zip_file()] {
        match run(archive, &["readme.txt", "nope.dds"]) {
            Err(FormatError::MissingEntries(m)) => assert_eq!(m, ["nope.dds"]),
            other => panic!("{other:?}"),
        }
    }
}

/// A caller's own error type: the callback returns it, and extraction's
/// own failures arrive as its `From<FormatError>`.
#[derive(Debug)]
enum CallerError {
    Stop(String),
    Format(FormatError),
}

impl From<FormatError> for CallerError {
    fn from(e: FormatError) -> Self {
        CallerError::Format(e)
    }
}

#[test]
fn callback_errors_stop_extraction() {
    let wanted = vec!["readme.txt".to_string(), "Data/b.esp".to_string()];
    let r = extract(solid_7z(), &wanted, |_, _| {
        Err(CallerError::Stop("stop".into()))
    });
    assert!(matches!(r, Err(CallerError::Stop(m)) if m == "stop"));
    let r = extract(solid_7z(), &["nope".to_string()], |_, _| {
        Err(CallerError::Stop("never called".into()))
    });
    assert!(
        matches!(r, Err(CallerError::Format(FormatError::MissingEntries(_)))),
        "{r:?}"
    );
}

/// Counts bytes read, to show unwanted 7z blocks are never decoded.
struct Counting(Vec<u8>, AtomicU64);
impl RangeRead for Counting {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.1.fetch_add(buf.len() as u64, Ordering::Relaxed);
        self.0.read_at(off, buf)
    }
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
}

#[test]
fn unwanted_7z_blocks_are_skipped() {
    let big = data(2 << 20, 23)
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ (i as u8).wrapping_mul(31))
        .collect::<Vec<u8>>();
    let archive = non_solid_7z(&big);
    let counting = std::sync::Arc::new(Counting(archive.clone(), AtomicU64::new(0)));
    let mut got = Vec::new();
    extract::<_, _, FormatError>(counting.clone(), &["small.txt".to_string()], |_, r| {
        r.read_to_end(&mut got)?;
        Ok(())
    })
    .unwrap();
    assert_eq!(got, b"small");
    let read = counting.1.load(Ordering::Relaxed);
    assert!(
        read < archive.len() as u64 / 4,
        "read {read} of {}",
        archive.len()
    );
}

#[test]
fn sniffs_formats_and_rejects_rar() {
    assert_eq!(sniff(&solid_7z()).unwrap(), ArchiveKind::SevenZip);
    assert_eq!(sniff(&zip_file()).unwrap(), ArchiveKind::Zip);
    let rar = b"Rar!\x1A\x07\x01\x00rest".to_vec();
    assert_eq!(sniff(&rar).unwrap(), ArchiveKind::Rar);
    assert!(matches!(
        run(rar, &["x"]),
        Err(FormatError::Unsupported { .. })
    ));
    assert!(matches!(
        sniff(&b"garbage!".to_vec()),
        Err(FormatError::Unsupported { .. })
    ));
    assert!(matches!(
        sniff(&Vec::new()),
        Err(FormatError::Unsupported { .. })
    ));
}

#[test]
fn corrupt_7z_data_fails_its_crc() {
    let mut a = non_solid_7z(&data(50_000, 24));
    // Flip a byte inside the first packed stream (right after the 32-byte signature header).
    a[40] ^= 0xFF;
    let r = run(a, &["big.bin"]);
    assert!(r.is_err(), "{r:?}");
}
