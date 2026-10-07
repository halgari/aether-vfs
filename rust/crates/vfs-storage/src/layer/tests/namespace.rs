//! Directory rules, removal, rename and attributes.

use super::*;

#[test]
fn remove_while_open_defers_the_store_delete_to_last_close() {
    let (s, _d) = temp_storage_every_close();
    let p = s.layer("l").unwrap();
    write_file(&p, "gone.txt", 0, b"still readable");
    let rec = s
        .catalog
        .get(s.catalog.layer_id("l").unwrap().unwrap(), "gone.txt")
        .unwrap()
        .unwrap();
    let id = layer_file_id(&rec.guid);
    let (h, _, _) = p.open(at("gone.txt"), OPEN_READ).unwrap();
    p.remove(at("gone.txt")).unwrap();
    assert!(p.getattr(at("gone.txt")).unwrap().is_none());
    assert_eq!(read_range(&p, h, 0, 14), b"still readable");
    assert!(s.store.stat(&id).unwrap().is_some());
    p.close(h).unwrap();
    assert!(
        s.store.stat(&id).unwrap().is_none(),
        "last close deletes it"
    );
}

#[test]
fn rename_over_a_file_deletes_the_replaced_data() {
    let (s, _d) = temp_storage_every_close();
    let p = s.layer("l").unwrap();
    write_file(&p, "a.txt", 0, b"new");
    write_file(&p, "b.txt", 0, b"old");
    let lid = s.catalog.layer_id("l").unwrap().unwrap();
    let old = layer_file_id(&s.catalog.get(lid, "b.txt").unwrap().unwrap().guid);
    p.rename(at("a.txt"), at("B.TXT")).unwrap();
    assert_eq!(read_file(&p, "b.txt"), b"new");
    let names: Vec<String> = p
        .readdir(at(""))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["B.TXT"]);
    // Replaced data goes at the next durable point.
    let (h, _, _) = p.open(at("b.txt"), OPEN_WRITE).unwrap();
    p.flush(h).unwrap();
    p.close(h).unwrap();
    assert!(s.store.stat(&old).unwrap().is_none());
}

#[test]
fn directory_rules() {
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    write_file(&p, "d/f.txt", 0, b"x");
    assert_eq!(p.remove(at("d")), Err(vfs_provider::ST_IS_DIR));
    assert_eq!(
        p.rename(at("d"), at("d/inner")),
        Err(vfs_provider::ST_BAD_REQUEST)
    );
    p.mkdir(at("e")).unwrap();
    assert_eq!(
        p.rename(at("d/f.txt"), at("e")),
        Err(vfs_provider::ST_EXISTS)
    );
    assert_eq!(
        p.open(at("d"), OPEN_WRITE).map(|_| ()),
        Err(vfs_provider::ST_IS_DIR)
    );
    assert_eq!(
        p.open(at("d/f.txt/x"), OPEN_WRITE | OPEN_CREATE)
            .map(|_| ()),
        Err(vfs_provider::ST_NOT_A_DIRECTORY)
    );
    assert_eq!(
        p.readdir(at("d/f.txt")),
        Err(vfs_provider::ST_NOT_A_DIRECTORY)
    );
    assert_eq!(p.readdir(at("nope")), Err(vfs_provider::ST_NOT_FOUND));
    p.mkdir(at("e"))
        .expect("mkdir of an existing directory is idempotent");
    assert_eq!(p.mkdir(at("d/f.txt")), Err(vfs_provider::ST_EXISTS));
    assert_eq!(p.remove(at("nope")), Err(vfs_provider::ST_NOT_FOUND));
    let (h, size, is_dir) = p.open(at("D"), OPEN_READ).unwrap();
    assert_eq!((size, is_dir), (0, true));
    p.close(h).unwrap();
}

#[test]
fn absurd_lengths_are_refused_without_poisoning_the_file() {
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    let (h, _, _) = p.open(at("f"), OPEN_WRITE | OPEN_CREATE).unwrap();
    assert_eq!(
        p.write_at(h, u64::MAX - 1, b"xy"),
        Err(vfs_provider::ST_BAD_REQUEST)
    );
    assert_eq!(p.write_at(h, 1 << 60, b"x"), Err(vfs_provider::ST_NO_SPACE));
    assert_eq!(p.set_len(h, 1 << 60), Err(vfs_provider::ST_NO_SPACE));
    p.write_at(h, 0, b"ok").unwrap();
    p.close(h).unwrap();
    assert_eq!(read_file(&p, "f"), b"ok");
}

#[test]
fn set_attr_size_and_mtime() {
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    write_file(&p, "f.txt", 0, b"hello world");
    p.set_attr(
        at("F.txt"),
        SetAttr {
            size: Some(5),
            mtime: Some(1_700_000_000),
        },
    )
    .unwrap();
    let st = p.getattr(at("f.txt")).unwrap().unwrap();
    assert_eq!((st.size, st.mtime), (5, 1_700_000_000));
    assert_eq!(read_file(&p, "f.txt"), b"hello");
    assert_eq!(
        p.set_attr(
            at("nope"),
            SetAttr {
                size: Some(1),
                mtime: None
            }
        ),
        Err(vfs_provider::ST_NOT_FOUND)
    );
}

#[test]
fn truncate_on_open() {
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    write_file(&p, "f.txt", 0, &vec![3u8; BS as usize + 1]);
    let (h, size, _) = p.open(at("f.txt"), OPEN_WRITE | OPEN_TRUNC).unwrap();
    assert_eq!(size, 0);
    p.close(h).unwrap();
    assert_eq!(read_file(&p, "f.txt"), b"");
}

#[test]
fn the_same_layer_is_one_provider_and_in_use_while_alive() {
    let (s, _d) = temp_storage();
    let a = s.layer("one").unwrap();
    let b = s.layer("one").unwrap();
    let _c = s.layer("two").unwrap();
    // One namespace: a write through one is visible through the other,
    // uncommitted.
    let (h, _, _) = a.open(at("f"), OPEN_WRITE | OPEN_CREATE).unwrap();
    a.write_at(h, 0, b"abc").unwrap();
    assert_eq!(b.getattr(at("f")).unwrap().unwrap().size, 3);
    a.close(h).unwrap();
    assert_eq!(s.layers_in_use(), ["one", "two"]);
    drop(a);
    drop(b);
    assert_eq!(s.layers_in_use(), ["two"]);
    // Reopening finds the same layer.
    let a = s.layer("one").unwrap();
    assert_eq!(read_file(&a, "f"), b"abc");
}
