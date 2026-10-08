mod common;

use aether_archive::FormatError;
use aether_archive::seekable::{self, FOOTER_LEN, SeekTable, decompress_frame, read_range};
use common::{lcg, pattern, seekable_zstd};

#[test]
fn parses_table_with_checksums() {
    let data = pattern(10_000);
    let s = seekable_zstd(&data, 4096, true);
    let t = SeekTable::read_from(&s[..]).unwrap();
    assert!(t.has_checksums());
    assert_eq!(t.frames().len(), 3);
    assert_eq!(t.decompressed_len(), 10_000);
    assert_eq!(t.table_len(), 8 + 3 * 12 + 9);
    assert_eq!(t.compressed_len() + t.table_len(), s.len() as u64);
    let f = &t.frames()[2];
    assert_eq!(
        (f.index, f.decompressed_offset, f.decompressed_size),
        (2, 8192, 1808)
    );
    assert_eq!(
        f.compressed_offset,
        (t.frames()[0].compressed_size + t.frames()[1].compressed_size) as u64
    );
}

#[test]
fn parses_table_without_checksums_from_tail_bytes() {
    let data = lcg(5000);
    let s = seekable_zstd(&data, 1000, false);
    let table_len = SeekTable::table_frame_len(&s[s.len() - FOOTER_LEN..]).unwrap();
    assert_eq!(table_len, 8 + 5 * 8 + 9);
    // parse() accepts any tail that contains the whole table.
    let t = SeekTable::parse(&s[s.len() - table_len as usize - 3..]).unwrap();
    assert!(!t.has_checksums());
    assert!(t.frames().iter().all(|f| f.checksum.is_none()));
    assert_eq!(t.decompressed_len(), 5000);
}

#[test]
fn empty_stream_has_no_frames() {
    let s = seekable_zstd(&[], 4096, true);
    let t = SeekTable::read_from(&s[..]).unwrap();
    assert_eq!(t.frames().len(), 0);
    assert_eq!(t.decompressed_len(), 0);
    assert!(t.frames_for_range(0..0).unwrap().is_empty());
    assert!(read_range(&s[..], &t, 0..0).unwrap().is_empty());
}

#[test]
fn maps_ranges_to_frames() {
    let data = pattern(10_000);
    let s = seekable_zstd(&data, 4096, true);
    let t = SeekTable::read_from(&s[..]).unwrap();
    let idx = |r: std::ops::Range<u64>| {
        t.frames_for_range(r)
            .unwrap()
            .iter()
            .map(|f| f.index)
            .collect::<Vec<_>>()
    };
    assert_eq!(idx(0..1), vec![0]);
    assert_eq!(idx(4095..4096), vec![0]);
    assert_eq!(idx(4096..4097), vec![1]);
    assert_eq!(idx(4095..4097), vec![0, 1]);
    assert_eq!(idx(0..10_000), vec![0, 1, 2]);
    assert_eq!(idx(9_999..10_000), vec![2]);
    assert_eq!(idx(5000..5000), Vec::<usize>::new());
    assert!(matches!(
        t.frames_for_range(0..10_001),
        Err(FormatError::Invalid { .. })
    ));
}

#[test]
fn reads_arbitrary_ranges() {
    let data = lcg(20_000);
    let s = seekable_zstd(&data, 3000, true);
    let t = SeekTable::read_from(&s[..]).unwrap();
    for (a, b) in [
        (0u64, 20_000u64),
        (1, 2),
        (2999, 3001),
        (5000, 17_000),
        (19_999, 20_000),
    ] {
        assert_eq!(
            read_range(&s[..], &t, a..b).unwrap(),
            &data[a as usize..b as usize],
            "{a}..{b}"
        );
    }
}

#[test]
fn decompresses_single_frame_and_checks_checksum() {
    let data = pattern(8192);
    let s = seekable_zstd(&data, 4096, true);
    let t = SeekTable::read_from(&s[..]).unwrap();
    let f = t.frames()[1].clone();
    let bytes = &s[f.compressed_range().start as usize..f.compressed_range().end as usize];
    assert_eq!(decompress_frame(&f, bytes).unwrap(), &data[4096..]);
    let mut bad = f.clone();
    bad.checksum = Some(f.checksum.unwrap() ^ 1);
    assert!(matches!(
        decompress_frame(&bad, bytes),
        Err(FormatError::Checksum { .. })
    ));
    let mut wrong_size = f.clone();
    wrong_size.decompressed_size -= 1;
    assert!(decompress_frame(&wrong_size, bytes).is_err());
}

#[test]
fn rejects_corrupt_tables() {
    let s = seekable_zstd(&pattern(9000), 4096, true);
    // Wrong seekable magic.
    let mut bad = s.clone();
    let n = bad.len();
    bad[n - 1] ^= 0xFF;
    assert!(SeekTable::read_from(&bad[..]).is_err());
    // Reserved descriptor bit.
    let mut bad = s.clone();
    bad[n - 5] |= 0x04;
    assert!(matches!(
        SeekTable::read_from(&bad[..]),
        Err(FormatError::Unsupported { .. })
    ));
    // Frame count claims more frames than the stream holds.
    let mut bad = s.clone();
    bad[n - 9..n - 5].copy_from_slice(&1_000_000u32.to_le_bytes());
    assert!(SeekTable::read_from(&bad[..]).is_err());
    // Extra junk before the frames: sizes no longer add up.
    let mut bad = vec![0u8; 7];
    bad.extend_from_slice(&s);
    assert!(SeekTable::read_from(&bad[..]).is_err());
    // Truncated.
    assert!(SeekTable::read_from(&s[..5]).is_err());
}

#[test]
fn decompress_frame_rejects_short_input() {
    let s = seekable_zstd(&pattern(100), 4096, false);
    let t = SeekTable::read_from(&s[..]).unwrap();
    let f = &t.frames()[0];
    assert!(seekable::decompress_frame(f, &s[..f.compressed_size as usize - 1]).is_err());
}

#[test]
fn zero_size_frames_are_skipped() {
    // A seekable stream may interleave frames that decompress to nothing
    // (for example skippable frames). They must never be returned or read.
    let a = pattern(1000);
    let b = lcg(1000);
    let fa = zstd::bulk::compress(&a, 3).unwrap();
    let fe = zstd::bulk::compress(&[], 3).unwrap();
    let fb = zstd::bulk::compress(&b, 3).unwrap();
    let mut s = Vec::new();
    let mut entries = Vec::new();
    for (f, d) in [(&fa, a.len()), (&fe, 0), (&fb, b.len())] {
        s.extend_from_slice(f);
        entries.extend_from_slice(&(f.len() as u32).to_le_bytes());
        entries.extend_from_slice(&(d as u32).to_le_bytes());
    }
    s.extend_from_slice(&0x184D_2A5Eu32.to_le_bytes());
    s.extend_from_slice(&(entries.len() as u32 + 9).to_le_bytes());
    s.extend_from_slice(&entries);
    s.extend_from_slice(&3u32.to_le_bytes());
    s.push(0);
    s.extend_from_slice(&0x8F92_EAB1u32.to_le_bytes());
    let t = SeekTable::read_from(&s[..]).unwrap();
    let idx: Vec<_> = t
        .frames_for_range(999..1001)
        .unwrap()
        .iter()
        .map(|f| f.index)
        .collect();
    assert_eq!(idx, [0, 2]);
    let mut want = a.clone();
    want.extend_from_slice(&b);
    assert_eq!(read_range(&s[..], &t, 0..2000).unwrap(), want);
}

#[test]
fn rejects_frames_over_one_gib() {
    let mut s = Vec::new();
    s.extend_from_slice(&0x184D_2A5Eu32.to_le_bytes());
    s.extend_from_slice(&(8u32 + 9).to_le_bytes());
    s.extend_from_slice(&10u32.to_le_bytes()); // compressed size
    s.extend_from_slice(&((1u32 << 30) + 1).to_le_bytes()); // decompressed size
    s.extend_from_slice(&1u32.to_le_bytes());
    s.push(0);
    s.extend_from_slice(&0x8F92_EAB1u32.to_le_bytes());
    assert!(matches!(
        SeekTable::parse(&s),
        Err(FormatError::Unsupported { .. })
    ));
    assert!(matches!(
        SeekTable::single_frame(10, (1 << 30) + 1),
        Err(FormatError::Unsupported { .. })
    ));
    let huge = seekable::FrameEntry {
        index: 0,
        compressed_offset: 0,
        compressed_size: 4,
        decompressed_offset: 0,
        decompressed_size: u32::MAX,
        checksum: None,
    };
    assert!(matches!(
        decompress_frame(&huge, &[0; 4]),
        Err(FormatError::Unsupported { .. })
    ));
}

#[test]
fn decompress_frame_into_fills_the_buffer() {
    let data = lcg(9000);
    let s = seekable_zstd(&data, 4096, true);
    let t = SeekTable::read_from(&s[..]).unwrap();
    let f = t.frames()[1].clone();
    let bytes = &s[f.compressed_range().start as usize..f.compressed_range().end as usize];
    let mut out = vec![0u8; 4096];
    seekable::decompress_frame_into(&f, bytes, &mut out).unwrap();
    assert_eq!(out, &data[4096..8192]);
    let mut short = vec![0u8; 4095];
    assert!(matches!(
        seekable::decompress_frame_into(&f, bytes, &mut short),
        Err(FormatError::Invalid { .. })
    ));
    let mut bad = f.clone();
    bad.checksum = Some(f.checksum.unwrap() ^ 1);
    assert!(matches!(
        seekable::decompress_frame_into(&bad, bytes, &mut out),
        Err(FormatError::Checksum { .. })
    ));
}

#[test]
fn single_frame_table_decodes_a_plain_zstd_frame() {
    let data = pattern(5000);
    let z = zstd::bulk::compress(&data, 3).unwrap();
    let t = SeekTable::single_frame(z.len() as u64, 5000).unwrap();
    assert_eq!(t.frames().len(), 1);
    assert_eq!(
        (t.compressed_len(), t.decompressed_len(), t.table_len()),
        (z.len() as u64, 5000, 0)
    );
    assert_eq!(read_range(&z[..], &t, 10..4000).unwrap(), &data[10..4000]);
    // Nexus frames carry zstd's content checksum (last 4 bytes of the frame);
    // a corrupted checksum must fail the read.
    let mut enc = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
    enc.include_checksum(true).unwrap();
    std::io::Write::write_all(&mut enc, &data).unwrap();
    let mut bad = enc.finish().unwrap();
    let tb = SeekTable::single_frame(bad.len() as u64, 5000).unwrap();
    assert_eq!(read_range(&bad[..], &tb, 0..5000).unwrap(), data);
    let n = bad.len();
    bad[n - 1] ^= 0x55;
    let tb = SeekTable::single_frame(bad.len() as u64, 5000).unwrap();
    assert!(read_range(&bad[..], &tb, 0..5000).is_err());
}
