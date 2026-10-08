//! Reads, writes, truncation, handles and the basic provider contract.

use super::*;

#[test]
fn put_files_writes_a_batch_whole_and_replaces_files() {
    let (s, d) = temp_storage();
    let p = s.layer("batch").unwrap();
    write_file(&p, "c/old", 0, b"the old bytes, long gone");
    let big: Vec<u8> = (0..3 * BS as usize + 5).map(|i| (i % 251) as u8).collect();
    s.put_files(
        "batch",
        &[
            ("c/old", b"new"),
            ("c/Big", &big),
            ("deep/er/x", b"x"),
            ("c/empty", b""),
        ],
    )
    .unwrap();
    assert_eq!(read_file(&p, "c/old"), b"new");
    assert_eq!(read_file(&p, "c/big"), big);
    assert_eq!(read_file(&p, "deep/er/x"), b"x");
    assert_eq!(read_file(&p, "c/empty"), b"");
    let st = p.getattr(at("c/Big")).unwrap().unwrap();
    assert_eq!((st.kind, st.size), (KIND_FILE, big.len() as u64));
    assert_eq!(
        p.stored_name(at("c/big")).unwrap().as_deref(),
        Some("Big"),
        "the name as given"
    );
    // A directory in the way, or a path twice: nothing is written.
    assert!(
        s.put_files("batch", &[("c/new", b"1"), ("deep", b"2")])
            .is_err()
    );
    assert!(
        s.put_files("batch", &[("c/two", b"1"), ("C/TWO", b"2")])
            .is_err()
    );
    assert!(p.getattr(at("c/new")).unwrap().is_none());
    assert!(p.getattr(at("c/two")).unwrap().is_none());
    // Durable at the next durable point, and whole after a reopen.
    drop(p);
    s.close().unwrap();
    let s = Storage::open(d.path(), cfg()).unwrap();
    let p = s.layer("batch").unwrap();
    assert_eq!(read_file(&p, "c/old"), b"new");
    assert_eq!(read_file(&p, "c/big"), big);
    assert!(s.store.verify().unwrap().is_ok());
}

#[test]
fn folded_path_is_the_parsed_paths_fold() {
    for rel in [
        "",
        "a",
        "Data/Meshes/Actor.NIF",
        "Data\\SKSE\\Plugins/x.ini",
        "/lead//double///and/trail/",
        "\\\\server\\share",
        "ÄÖ/İstanbul/\u{212A}.txt",
        "a/./b",
        "a/../b",
        "..",
        ".",
        "a/.../b",
        "a/.hidden/..b",
    ] {
        assert_eq!(
            folded_path(rel),
            LPath::parse(rel).map(|p| p.folded),
            "{rel:?}"
        );
    }
}

#[test]
fn capabilities_are_read_write_insensitive_with_the_block_hint() {
    let (s, _d) = temp_storage();
    let c = s.layer("caps").unwrap().capabilities();
    assert_eq!(c.access, vfs_provider::Access::ReadWrite);
    assert!(!c.immutable && !c.slow);
    assert_eq!(c.preferred_block, Some(BS as u32));
    assert_eq!(c.case, vfs_provider::CaseMatch::Insensitive);
}

#[test]
fn conformance() {
    let (s, _d) = temp_storage();
    let p = s.layer("conf").unwrap();
    seed_fixture(&p);
    vfs_provider::assert_conformance(p);
}

#[test]
fn partial_block_write_preserves_neighbours() {
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    let mut want = vec![0xAAu8; 3 * BS as usize];
    write_file(&p, "big.bin", 0, &want);

    let (h, _, _) = p.open(at("big.bin"), OPEN_WRITE).unwrap();
    p.write_at(h, BS + 10, b"XYZ").unwrap();
    // Read across the edited block through the writing handle (uncommitted)...
    want[BS as usize + 10..BS as usize + 13].copy_from_slice(b"XYZ");
    assert_eq!(read_range(&p, h, 0, want.len()), want);
    p.close(h).unwrap();
    // ...and after it committed.
    assert_eq!(read_file(&p, "big.bin"), want);
}

#[test]
fn gap_reads_as_zeros() {
    let (s, d) = temp_storage();
    let p = s.layer("l").unwrap();
    let off = 5 * BS + 7;
    let mut want = vec![0u8; off as usize];
    want.extend_from_slice(b"end");

    let (h, _, _) = p.open(at("gap.bin"), OPEN_WRITE | OPEN_CREATE).unwrap();
    p.write_at(h, off, b"end").unwrap();
    assert_eq!(read_range(&p, h, 0, want.len() + 10), want, "before commit");
    p.close(h).unwrap();
    assert_eq!(read_file(&p, "gap.bin"), want, "after close");

    drop(p);
    s.close().unwrap();
    let s = Storage::open(d.path(), cfg()).unwrap();
    let p = s.layer("l").unwrap();
    assert_eq!(read_file(&p, "gap.bin"), want, "after reopen");
}

#[test]
fn truncate_then_grow_zero_fills() {
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    write_file(&p, "t.bin", 0, &vec![0xAAu8; 3 * BS as usize]);

    let (h, _, _) = p.open(at("t.bin"), OPEN_WRITE).unwrap();
    p.set_len(h, BS + 5).unwrap();
    p.set_len(h, 3 * BS).unwrap();
    p.close(h).unwrap();

    let got = read_file(&p, "t.bin");
    let mut want = vec![0xAAu8; BS as usize + 5];
    want.resize(3 * BS as usize, 0);
    assert_eq!(got.len(), want.len());
    assert!(got == want, "the tail past bs + 5 must be zeros");
}

#[test]
fn set_len_within_an_uncommitted_extension() {
    // Grow by a write (uncommitted), shrink back into the extension, grow
    // again: the dropped bytes must not come back.
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    write_file(&p, "x.bin", 0, b"abc");
    let (h, _, _) = p.open(at("x.bin"), OPEN_WRITE).unwrap();
    p.write_at(h, 3, &[0x55; 100]).unwrap();
    p.set_len(h, 10).unwrap();
    p.set_len(h, 50).unwrap();
    p.close(h).unwrap();
    let mut want = b"abc".to_vec();
    want.extend_from_slice(&[0x55; 7]);
    want.resize(50, 0);
    assert_eq!(read_file(&p, "x.bin"), want);
}

#[test]
fn many_dirty_blocks_commit_mid_handle_and_read_back() {
    // More than 4 dirty blocks forces commits while the handle stays open.
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    let body: Vec<u8> = (0..(12 * BS + 99)).map(|i| (i % 251) as u8).collect();
    let (h, _, _) = p.open(at("seq.bin"), OPEN_WRITE | OPEN_CREATE).unwrap();
    for (i, chunk) in body.chunks(1000).enumerate() {
        p.write_at(h, i as u64 * 1000, chunk).unwrap();
    }
    assert_eq!(read_range(&p, h, 0, body.len()), body);
    p.close(h).unwrap();
    assert_eq!(read_file(&p, "seq.bin"), body);
}

#[test]
fn second_handle_sees_uncommitted_writes() {
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    let (h1, _, _) = p.open(at("shared.txt"), OPEN_WRITE | OPEN_CREATE).unwrap();
    p.write_at(h1, 0, b"0123456789").unwrap();
    let (h2, size, _) = p.open(at("shared.txt"), OPEN_READ).unwrap();
    assert_eq!(size, 10);
    assert_eq!(read_range(&p, h2, 0, 10), b"0123456789");
    assert_eq!(p.getattr(at("shared.txt")).unwrap().unwrap().size, 10);
    p.close(h2).unwrap();
    p.close(h1).unwrap();
}

#[test]
fn rename_moves_without_copying() {
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    p.mkdir(at("dir")).unwrap();
    let body: Vec<u8> = (0..2 * BS).map(|i| (i % 7) as u8).collect();
    write_file(&p, "dir/f.bin", 0, &body);

    let before = s.store.stats().unwrap();
    p.rename(at("dir"), at("moved")).unwrap();
    let after = s.store.stats().unwrap();
    assert_eq!(before.unflushed_bytes, after.unflushed_bytes);
    assert_eq!(before.unflushed_commits, after.unflushed_commits);
    let live = |st: &aether_block_store::Stats| st.packs.iter().map(|p| p.live_bytes).sum::<u64>();
    assert_eq!(live(&before), live(&after));

    assert!(p.getattr(at("dir/f.bin")).unwrap().is_none());
    assert_eq!(read_file(&p, "moved/f.bin"), body);
}

#[test]
fn open_handle_survives_rename_of_its_directory() {
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    let (h, _, _) = p.open(at("d/f.txt"), OPEN_WRITE | OPEN_CREATE).unwrap();
    p.write_at(h, 0, b"one").unwrap();
    p.rename(at("d"), at("e")).unwrap();
    p.write_at(h, 3, b"two").unwrap();
    p.close(h).unwrap();
    assert_eq!(read_file(&p, "e/f.txt"), b"onetwo");
    assert_eq!(p.getattr(at("e/f.txt")).unwrap().unwrap().size, 6);
    assert!(p.getattr(at("d/f.txt")).unwrap().is_none());
    assert!(p.getattr(at("d")).unwrap().is_none());
}

#[test]
fn stored_name_is_the_catalog_rows_spelling_for_any_query_case() {
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    p.mkdir(at("Saves")).unwrap();
    write_file(&p, "Saves/One.ESS", 0, b"x");
    let name = |q: &str| p.stored_name(at(q)).expect("answered, not unsupported");
    assert_eq!(name("saves").as_deref(), Some("Saves"));
    assert_eq!(name("SAVES/one.ess").as_deref(), Some("One.ESS"));
    assert_eq!(name("saves/missing"), None);
    assert_eq!(name(""), None);
    // A rename to another spelling is seen at once.
    p.rename(at("Saves/One.ESS"), at("Saves/two.Ess")).unwrap();
    assert_eq!(name("saves/ONE.ess"), None);
    assert_eq!(name("saves/TWO.ESS").as_deref(), Some("two.Ess"));
}

#[test]
fn missing_committed_block_is_an_io_error() {
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    write_file(&p, "f.bin", 0, &vec![1u8; 2 * BS as usize]);
    let rec = s
        .catalog
        .get(s.catalog.layer_id("l").unwrap().unwrap(), "f.bin")
        .unwrap()
        .unwrap();
    s.ram.invalidate_file(&layer_file_id(&rec.guid));
    s.store.delete(&layer_file_id(&rec.guid)).unwrap();

    let (h, _, _) = p.open(at("f.bin"), OPEN_READ).unwrap();
    assert_eq!(p.read_at(h, 0, &mut [0u8; 16]), Err(ST_IO_ERROR));
    p.close(h).unwrap();
}

#[test]
fn case_insensitive_lookup_preserves_case() {
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    write_file(&p, "Saves/One.ESS", 0, b"x");
    let st = p.getattr(at("saves/one.ess")).unwrap().unwrap();
    assert_eq!((st.kind, st.size), (KIND_FILE, 1));
    let names: Vec<String> = p
        .readdir(at("SAVES"))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["One.ESS"]);
    let root: Vec<(String, u8)> = p
        .readdir(at(""))
        .unwrap()
        .into_iter()
        .map(|e| (e.name, e.stat.kind))
        .collect();
    assert_eq!(root, [("Saves".to_string(), KIND_DIR)]);
}
