//! The parse-from-bytes zip API used by async callers that do their own
//! range I/O, and the Nexus `0x4E58` seek-table extra field.
mod common;

use aether_archive::FormatError;
use aether_archive::seekable::SeekTable;
use aether_archive::zip::{
    CentralDirectory, EndRecords, LOCAL_HEADER_LEN, MAX_TAIL, PLAIN_FRAME_MAX, SeekableEntry,
    ZIP64_EOCD_LEN, ZipEntry, ZipIndex, entry_data_range, local_data_range,
    locate_central_directory,
};
use common::{TestEntry, build_zip, lcg, nexus_extra, pattern};

/// Parse `zip` the way an HTTP client would: tail, (ZIP64 record), directory.
fn index_from_parts(zip: &[u8]) -> ZipIndex {
    let tail_start = zip.len().saturating_sub(MAX_TAIL as usize);
    let cd = match locate_central_directory(&zip[tail_start..], tail_start as u64).unwrap() {
        EndRecords::Found(cd) => cd,
        EndRecords::NeedZip64Record(pos) => {
            let p = pos as usize;
            CentralDirectory::from_zip64_record(
                &zip[p..p + ZIP64_EOCD_LEN as usize],
                zip.len() as u64,
            )
            .unwrap()
        }
    };
    let bytes = &zip[cd.offset as usize..(cd.offset + cd.size) as usize];
    ZipIndex::parse_central_directory(bytes, cd.entries).unwrap()
}

#[test]
fn parts_api_agrees_with_read() {
    for zip64 in [false, true] {
        let zip = build_zip(
            b"stub",
            &[
                TestEntry::stored("readme.txt", &pattern(1234)),
                TestEntry::seekable("Data/meshes/a.nif", &lcg(50_000), 16_384),
            ],
            zip64,
            b"a comment",
        );
        let a = ZipIndex::read(&zip[..]).unwrap();
        let b = index_from_parts(&zip);
        assert_eq!(a.entries(), b.entries(), "zip64={zip64}");
        for e in b.entries() {
            let lh = e.local_header_offset as usize;
            let range = local_data_range(&zip[lh..lh + LOCAL_HEADER_LEN as usize], e).unwrap();
            assert_eq!(range, entry_data_range(&zip[..], e).unwrap());
        }
    }
}

#[test]
fn zip64_record_before_the_tail_is_requested_separately() {
    // A file longer than MAX_TAIL whose locator points at a copy of the
    // ZIP64 record near the start (legal: the record need not be adjacent).
    let mut zip = build_zip(
        &vec![0u8; 70_000],
        &[TestEntry::stored("a", b"x")],
        true,
        b"",
    );
    let n = zip.len();
    let (eocd, loc) = (n - 22, n - 22 - 20);
    let rec = zip[loc - 56..loc].to_vec();
    zip[100..156].copy_from_slice(&rec);
    zip[loc + 8..loc + 16].copy_from_slice(&100u64.to_le_bytes());
    let tail_start = n - MAX_TAIL as usize;
    assert_eq!(
        locate_central_directory(&zip[tail_start..], tail_start as u64).unwrap(),
        EndRecords::NeedZip64Record(100)
    );
    assert_eq!(index_from_parts(&zip).entries()[0].name, "a");
    assert_eq!(ZipIndex::read(&zip[..]).unwrap().entries()[0].name, "a");
    assert!(eocd > loc);
}

#[test]
fn locate_rejects_short_tails_and_out_of_range_directories() {
    let zip = build_zip(
        &vec![0u8; 70_000],
        &[TestEntry::stored("a", b"x")],
        false,
        b"",
    );
    let short = zip.len() - 1000;
    assert!(matches!(
        locate_central_directory(&zip[short..], short as u64),
        Err(FormatError::Invalid { .. })
    ));
    // Directory offset past EOF.
    let mut bad = zip.clone();
    let eocd = bad.len() - 22;
    bad[eocd + 16..eocd + 20].copy_from_slice(&(u32::MAX - 5).to_le_bytes());
    let start = bad.len() - MAX_TAIL as usize;
    assert!(matches!(
        locate_central_directory(&bad[start..], start as u64),
        Err(FormatError::Invalid { .. })
    ));
}

#[test]
fn tolerates_bytes_after_the_comment() {
    let mut zip = build_zip(&[], &[TestEntry::stored("a.txt", b"hi")], true, b"{}");
    zip.extend_from_slice(b"trailing junk");
    let idx = ZipIndex::read(&zip[..]).unwrap();
    assert_eq!(idx.find("A.TXT").unwrap().name, "a.txt");
}

#[test]
fn nexus_extra_field_carries_the_seek_table() {
    let big = lcg(70_000);
    let zip = build_zip(
        &[],
        &[
            TestEntry::nexus("tex/a.dds", &big, 16_384),
            TestEntry::stored("dir/", &[]),
        ],
        true,
        b"",
    );
    let idx = index_from_parts(&zip);
    let e = &idx.entries()[0];
    let t = e.seek_table.clone().expect("0x4E58 parsed");
    assert_eq!(t.frames().len(), 5);
    assert!(t.has_checksums());
    assert_eq!(idx.entries()[1].seek_table, None);

    // The directory's copy is the table at the end of the entry's data.
    let data = entry_data_range(&zip[..], e).unwrap();
    let window = &zip[data.start as usize..data.end as usize];
    assert_eq!(SeekTable::read_from(window).unwrap(), t);
    assert_eq!(e.frame_map().unwrap(), Some(t.clone()));

    let s = SeekableEntry::from_parts(e.clone(), data.clone(), t).unwrap();
    assert_eq!(
        s.read(&zip[..], 16_000..40_000).unwrap(),
        &big[16_000..40_000]
    );
    let f = s.frames_for_range(16_384..16_385).unwrap();
    assert_eq!(f[0].index, 1);
    // from_parts refuses a data range that disagrees with the directory.
    assert!(
        SeekableEntry::from_parts(e.clone(), data.start..data.end - 1, s.table.clone()).is_err()
    );
}

#[test]
fn corrupt_nexus_extra_field_is_rejected() {
    // The seek table of a different stream: sizes do not match the entry.
    let other = TestEntry::nexus("x", &pattern(9_000), 4096);
    let mut e = TestEntry::seekable("a.bin", &lcg(20_000), 4096);
    e.central_extra = other.central_extra.clone();
    let zip = build_zip(&[], &[e], true, b"");
    assert!(matches!(
        ZipIndex::read(&zip[..]),
        Err(FormatError::Invalid { .. })
    ));
    // Not a seek table at all.
    let mut e = TestEntry::stored("b.bin", b"abc");
    e.central_extra = nexus_extra(b"garbage bytes");
    let zip = build_zip(&[], &[e], true, b"");
    assert!(ZipIndex::read(&zip[..]).is_err());
}

#[test]
fn small_zstd_entries_are_one_plain_frame() {
    let data = pattern(40_000);
    let e = TestEntry {
        name: b"plain.esp".to_vec(),
        method: 93,
        flags: 0x0808,
        stored: zstd::bulk::compress(&data, 3).unwrap(),
        uncompressed_size: data.len() as u64,
        central_extra: Vec::new(),
    };
    let zip = build_zip(&[], &[e], true, b"");
    let idx = ZipIndex::read(&zip[..]).unwrap();
    let entry = &idx.entries()[0];
    let t = entry.frame_map().unwrap().unwrap();
    assert_eq!(t.frames().len(), 1);
    assert_eq!(t.table_len(), 0);
    let s = SeekableEntry::from_parts(entry.clone(), entry_data_range(&zip[..], entry).unwrap(), t)
        .unwrap();
    assert_eq!(s.read(&zip[..], 100..30_000).unwrap(), &data[100..30_000]);
}

#[test]
fn frame_map_needs_zstd_and_reports_missing_tables() {
    let mut big = TestEntry::seekable("big.bin", &pattern(100), 64);
    big.uncompressed_size = PLAIN_FRAME_MAX + 1; // claims > 4 MiB, no 0x4E58
    let zip = build_zip(&[], &[big, TestEntry::stored("s.txt", b"s")], true, b"");
    let idx = ZipIndex::read(&zip[..]).unwrap();
    assert_eq!(idx.entries()[0].frame_map().unwrap(), None);
    assert!(matches!(
        idx.entries()[1].frame_map(),
        Err(FormatError::Unsupported { .. })
    ));
}

#[test]
fn local_data_range_rejects_an_offset_that_would_overflow() {
    // A corrupt central directory can claim any local_header_offset; the
    // sums built from it must not panic.
    let mut header = vec![0u8; LOCAL_HEADER_LEN as usize];
    header[0..4].copy_from_slice(&0x0403_4b50u32.to_le_bytes());
    let e = ZipEntry {
        name: "x".into(),
        method: 0,
        flags: 0,
        crc32: 0,
        compressed_size: 10,
        uncompressed_size: 10,
        local_header_offset: u64::MAX - 5,
        seek_table: None,
    };
    assert!(matches!(
        local_data_range(&header, &e),
        Err(FormatError::Invalid { .. })
    ));
}

#[test]
fn locate_central_directory_rejects_a_tail_start_that_would_overflow() {
    let tail = vec![0u8; 30];
    assert!(matches!(
        locate_central_directory(&tail, u64::MAX - 5),
        Err(FormatError::Invalid { .. })
    ));
}

#[test]
fn find_folds_unicode_case_and_separators() {
    let mut e = TestEntry::stored("x", b"1");
    e.name = "Textures/Ärmel/Ü.dds".as_bytes().to_vec();
    e.flags = 0x0800;
    let zip = build_zip(&[], &[TestEntry::stored("a/b.txt", b"0"), e], false, b"");
    let idx = ZipIndex::read(&zip[..]).unwrap();
    assert_eq!(idx.position(r"TEXTURES\ärmel\ü.DDS"), Some(1));
    assert_eq!(idx.find("A\\B.TXT").unwrap().name, "a/b.txt");
    assert_eq!(idx.position("a/b.txt/"), None);
}
