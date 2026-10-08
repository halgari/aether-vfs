mod common;

use aether_archive::FormatError;
use aether_archive::zip::{METHOD_ZSTD, SeekableEntry, ZipIndex, entry_data_range, read_entry};
use common::{TestEntry, build_zip, lcg, pattern};

fn sample(zip64: bool) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let big = lcg(50_000);
    let small = pattern(1234);
    let zip = build_zip(
        b"stub",
        &[
            TestEntry::stored("readme.txt", &small),
            TestEntry::seekable("Data/meshes/a.nif", &big, 16_384),
            TestEntry::stored("Data/", &[]),
        ],
        zip64,
        b"a comment",
    );
    (zip, small, big)
}

#[test]
fn lists_entries() {
    for zip64 in [false, true] {
        let (zip, small, big) = sample(zip64);
        let idx = ZipIndex::read(&zip[..]).unwrap();
        let names: Vec<_> = idx.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            ["readme.txt", "Data/meshes/a.nif", "Data/"],
            "zip64={zip64}"
        );
        let e = &idx.entries()[1];
        assert_eq!(e.method, METHOD_ZSTD);
        assert_eq!(e.uncompressed_size, big.len() as u64);
        assert_eq!(idx.entries()[0].uncompressed_size, small.len() as u64);
        assert!(idx.entries()[2].is_dir());
    }
}

#[test]
fn finds_entries_with_windows_paths() {
    let (zip, _, _) = sample(false);
    let idx = ZipIndex::read(&zip[..]).unwrap();
    assert_eq!(
        idx.find("data\\MESHES\\A.NIF").unwrap().name,
        "Data/meshes/a.nif"
    );
    assert!(idx.find("Data/meshes/b.nif").is_none());
}

#[test]
fn locates_and_reads_entry_data() {
    for zip64 in [false, true] {
        let (zip, small, big) = sample(zip64);
        let idx = ZipIndex::read(&zip[..]).unwrap();
        let r = entry_data_range(&zip[..], &idx.entries()[0]).unwrap();
        assert_eq!(&zip[r.start as usize..r.end as usize], &small[..]);
        assert_eq!(read_entry(&zip[..], &idx.entries()[0]).unwrap(), small);
        assert_eq!(
            read_entry(&zip[..], &idx.entries()[1]).unwrap(),
            big,
            "zstd whole-entry read, zip64={zip64}"
        );
        assert!(read_entry(&zip[..], &idx.entries()[2]).unwrap().is_empty());
    }
}

#[test]
fn seekable_entry_maps_and_reads_ranges() {
    for zip64 in [false, true] {
        let (zip, _, big) = sample(zip64);
        let idx = ZipIndex::read(&zip[..]).unwrap();
        let s = SeekableEntry::open(&zip[..], &idx.entries()[1]).unwrap();
        assert_eq!(s.table.frames().len(), 4);
        let frames = s.frames_for_range(16_000..17_000).unwrap();
        assert_eq!(frames.iter().map(|f| f.index).collect::<Vec<_>>(), [0, 1]);
        let abs = s.absolute(&frames[1]);
        assert_eq!(abs.start, s.data.start + frames[1].compressed_offset);
        assert_eq!(abs.end - abs.start, frames[1].compressed_size as u64);
        assert_eq!(
            s.read(&zip[..], 16_000..17_000).unwrap(),
            &big[16_000..17_000]
        );
        assert_eq!(s.read(&zip[..], 0..50_000).unwrap(), big);
    }
}

#[test]
fn seekable_entry_rejects_non_zstd_and_size_mismatch() {
    let (zip, _, _) = sample(false);
    let idx = ZipIndex::read(&zip[..]).unwrap();
    assert!(matches!(
        SeekableEntry::open(&zip[..], &idx.entries()[0]),
        Err(FormatError::Unsupported { .. })
    ));
    let mut e = TestEntry::seekable("x.bin", &pattern(100), 64);
    e.uncompressed_size = 99;
    let zip = build_zip(&[], &[e], false, &[]);
    let idx = ZipIndex::read(&zip[..]).unwrap();
    assert!(SeekableEntry::open(&zip[..], &idx.entries()[0]).is_err());
}

#[test]
fn decodes_cp437_and_utf8_names() {
    let mut cp437 = TestEntry::stored("x", b"1");
    cp437.name = vec![b'n', 0x82, b'.', b't']; // 0x82 = 'é' in CP437, invalid UTF-8 alone
    let mut utf8 = TestEntry::stored("x", b"2");
    utf8.name = "ü.txt".as_bytes().to_vec();
    utf8.flags = 0x0800;
    let zip = build_zip(&[], &[cp437, utf8], false, &[]);
    let idx = ZipIndex::read(&zip[..]).unwrap();
    assert_eq!(idx.entries()[0].name, "né.t");
    assert_eq!(idx.entries()[1].name, "ü.txt");
}

#[test]
fn agrees_with_zip_crate_on_a_real_writer_output() {
    use std::io::Write;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    w.start_file("a/b.txt", opts).unwrap();
    w.write_all(b"hello").unwrap();
    let opts64 = opts.large_file(true);
    w.start_file("c.bin", opts64).unwrap();
    w.write_all(&pattern(3000)).unwrap();
    let bytes = w.finish().unwrap().into_inner();
    let idx = ZipIndex::read(&bytes[..]).unwrap();
    assert_eq!(
        read_entry(&bytes[..], idx.find("a/b.txt").unwrap()).unwrap(),
        b"hello"
    );
    assert_eq!(
        read_entry(&bytes[..], idx.find("c.bin").unwrap()).unwrap(),
        pattern(3000)
    );
}

#[test]
fn rejects_garbage_and_truncation() {
    assert!(ZipIndex::read(&b"not a zip at all, definitely not"[..]).is_err());
    let (zip, _, _) = sample(false);
    // Cut off the end: no EOCD any more.
    assert!(ZipIndex::read(&zip[..zip.len() - 30]).is_err());
    // Central directory offset pointing past EOF.
    let mut bad = zip.clone();
    let eocd = bad.len() - 22 - 9;
    bad[eocd + 16..eocd + 20].copy_from_slice(&u32::MAX.wrapping_sub(5).to_le_bytes());
    assert!(ZipIndex::read(&bad[..]).is_err());
    // Entry count larger than the directory could hold.
    let mut bad = zip.clone();
    bad[eocd + 10..eocd + 12].copy_from_slice(&60_000u16.to_le_bytes());
    assert!(matches!(
        ZipIndex::read(&bad[..]),
        Err(FormatError::Invalid { .. })
    ));
}

#[test]
fn rejects_unsupported_methods_and_encryption() {
    let mut deflated = TestEntry::stored("d.txt", b"abc");
    deflated.method = 8;
    let mut enc = TestEntry::stored("e.txt", b"abc");
    enc.flags = 1;
    let zip = build_zip(&[], &[deflated, enc], false, &[]);
    let idx = ZipIndex::read(&zip[..]).unwrap();
    assert!(matches!(
        read_entry(&zip[..], &idx.entries()[0]),
        Err(FormatError::Unsupported { .. })
    ));
    assert!(matches!(
        entry_data_range(&zip[..], &idx.entries()[1]),
        Err(FormatError::Unsupported { .. })
    ));
}

#[test]
fn rejects_multi_disk_and_broken_zip64() {
    let (zip, _, _) = sample(false);
    let eocd = zip.len() - 22 - 9;
    let mut multi = zip.clone();
    multi[eocd + 4..eocd + 6].copy_from_slice(&1u16.to_le_bytes());
    assert!(matches!(
        ZipIndex::read(&multi[..]),
        Err(FormatError::Unsupported { .. })
    ));

    let (zip64, _, _) = sample(true);
    let eocd = zip64.len() - 22 - 9;
    // Locator points at garbage instead of the ZIP64 end record.
    let mut bad = zip64.clone();
    bad[eocd - 20 + 8..eocd - 20 + 16].copy_from_slice(&0u64.to_le_bytes());
    assert!(matches!(
        ZipIndex::read(&bad[..]),
        Err(FormatError::Invalid { .. })
    ));
    // ZIP64 markers without a locator.
    let mut nolocator = zip64.clone();
    nolocator[eocd - 20..eocd - 16].fill(0);
    assert!(matches!(
        ZipIndex::read(&nolocator[..]),
        Err(FormatError::Invalid { .. })
    ));
}

#[test]
fn finds_entries_stored_with_backslashes() {
    let zip = build_zip(
        &[],
        &[TestEntry::stored("Data\\Scripts\\x.pex", b"pex")],
        false,
        &[],
    );
    let idx = ZipIndex::read(&zip[..]).unwrap();
    assert_eq!(
        idx.find("data/scripts/X.PEX").unwrap().name,
        "Data\\Scripts\\x.pex"
    );
    assert_eq!(
        idx.find("DATA\\SCRIPTS\\x.pex").unwrap().name,
        "Data\\Scripts\\x.pex"
    );
}

#[test]
fn plain_zstd_entry_without_seek_table() {
    // Method 93 written by ordinary tools: one zstd frame, no seek table.
    let data = pattern(40_000);
    let e = TestEntry {
        name: b"plain.bin".to_vec(),
        method: 93,
        flags: 0,
        stored: zstd::bulk::compress(&data, 3).unwrap(),
        uncompressed_size: data.len() as u64,
        central_extra: Vec::new(),
    };
    let zip = build_zip(&[], &[e], false, &[]);
    let idx = ZipIndex::read(&zip[..]).unwrap();
    assert!(matches!(
        SeekableEntry::open(&zip[..], &idx.entries()[0]),
        Err(FormatError::Invalid { .. })
    ));
    assert_eq!(read_entry(&zip[..], &idx.entries()[0]).unwrap(), data);
}

#[test]
fn data_descriptor_entries_use_central_directory_sizes() {
    // Flag bit 3: the local header's CRC and sizes are zero; only the central
    // directory has them.
    let mut e = TestEntry::stored("streamed.txt", b"streamed data");
    e.flags = 0x0008;
    let mut zip = build_zip(&[], &[e], false, &[]);
    zip[14..26].fill(0); // local header crc32, compressed size, uncompressed size
    let idx = ZipIndex::read(&zip[..]).unwrap();
    assert_eq!(
        read_entry(&zip[..], &idx.entries()[0]).unwrap(),
        b"streamed data"
    );
}

#[test]
fn a_zstd_entry_claiming_u64_max_bytes_errors_without_overflow() {
    let data = pattern(1000);
    let e = TestEntry {
        name: b"huge.bin".to_vec(),
        method: 93,
        flags: 0,
        stored: zstd::bulk::compress(&data, 3).unwrap(),
        uncompressed_size: data.len() as u64,
        central_extra: Vec::new(),
    };
    let zip = build_zip(&[], &[e], false, &[]);
    let idx = ZipIndex::read(&zip[..]).unwrap();
    let mut entry = idx.entries()[0].clone();
    entry.uncompressed_size = u64::MAX;
    assert!(matches!(
        read_entry(&zip[..], &entry),
        Err(FormatError::Invalid { .. })
    ));
}
