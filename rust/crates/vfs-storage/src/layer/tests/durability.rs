//! Durable points, crashes, deferred deletes and failed commits.

use super::*;

/// Opens a kill-time copy of `d`'s storage and its layer `name`.
#[cfg(not(windows))]
pub(super) fn killed_copy(
    d: &std::path::Path,
    name: &str,
) -> (Arc<Storage>, Arc<dyn Provider>, tempfile::TempDir) {
    let killed = vfs_testkit::tempdir().unwrap();
    snapshot_as_killed(d, killed.path()).unwrap();
    let k = Storage::open(killed.path(), cfg()).unwrap();
    let kp = k.layer(name).unwrap();
    (k, kp, killed)
}

#[cfg(not(windows))]
pub(super) fn names(p: &Arc<dyn Provider>, dir: &str) -> Vec<String> {
    p.readdir(at(dir))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect()
}

/// The reviewer's reproduction: save to a temp file, rename it over the
/// real save, get killed. The rename must have been durable.
#[cfg(not(windows))]
#[test]
fn save_then_rename_over_is_durable() {
    let (s, d) = temp_storage_every_close();
    let p = s.layer("saves").unwrap();
    write_file(&p, "save.ess", 0, b"old save");
    write_file(&p, "save.tmp", 0, b"new save");
    p.rename(at("save.tmp"), at("save.ess")).unwrap();
    let (_k, kp, _kd) = killed_copy(d.path(), "saves");
    assert_eq!(names(&kp, ""), ["save.ess"]);
    assert_eq!(read_file(&kp, "save.ess"), b"new save");
    drop(p);
}

#[cfg(not(windows))]
#[test]
fn save_then_rename_to_a_fresh_name_is_durable() {
    let (s, d) = temp_storage_every_close();
    let p = s.layer("saves").unwrap();
    write_file(&p, "Saves/save5.tmp", 0, b"fifth");
    p.rename(at("Saves/save5.tmp"), at("Saves/save5.ess"))
        .unwrap();
    let (_k, kp, _kd) = killed_copy(d.path(), "saves");
    assert_eq!(names(&kp, "saves"), ["save5.ess"]);
    assert_eq!(read_file(&kp, "saves/save5.ess"), b"fifth");
}

#[cfg(not(windows))]
#[test]
fn remove_and_mkdir_are_durable() {
    let (s, d) = temp_storage_every_close();
    let p = s.layer("saves").unwrap();
    write_file(&p, "a.ess", 0, b"a");
    write_file(&p, "b.ess", 0, b"b");
    p.remove(at("a.ess")).unwrap();
    p.mkdir(at("Backups/Old")).unwrap();
    let (_k, kp, _kd) = killed_copy(d.path(), "saves");
    assert_eq!(names(&kp, ""), ["b.ess", "Backups"]); // folded key order
    assert_eq!(names(&kp, "backups"), ["Old"]);
    assert_eq!(read_file(&kp, "b.ess"), b"b");
}

#[test]
fn remove_deletes_the_store_file_at_once_when_closed() {
    let (s, _d) = temp_storage_every_close();
    let p = s.layer("l").unwrap();
    write_file(&p, "x", 0, b"x");
    let lid = s.catalog.layer_id("l").unwrap().unwrap();
    let id = layer_file_id(&s.catalog.get(lid, "x").unwrap().unwrap().guid);
    p.remove(at("x")).unwrap();
    assert!(s.store.stat(&id).unwrap().is_none());
}

/// A shrink whose commit fails must still drop the cut bytes: a later
/// grow (by `set_len` or a write) reads zeros there, before and after it
/// commits, never the store's stale copy.
#[test]
fn a_failed_shrink_commit_never_resurrects_the_cut_bytes() {
    for (cut, regrow) in [(BS + 5, "set_len"), (BS, "set_len"), (BS + 5, "write")] {
        let (s, _d) = temp_storage();
        let lid = s.catalog.create_layer("l").unwrap();
        let lp: Arc<LayerProvider> = Arc::new(LayerProvider::new(Arc::clone(&s), "l".into(), lid));
        let p: Arc<dyn Provider> = lp.clone();
        write_file(&p, "f", 0, &vec![0xAAu8; 3 * BS as usize]);
        let guid = s.catalog.get(lid, "f").unwrap().unwrap().guid;

        let (h, _, _) = p.open(at("f"), OPEN_WRITE).unwrap();
        let cell = lp.live_cell(&guid).unwrap();
        cell.fail_commit
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(p.set_len(h, cut), Err(ST_IO_ERROR), "{cut} {regrow}");
        let mut want = vec![0xAAu8; cut as usize];
        want.resize(3 * BS as usize, 0);
        if regrow == "set_len" {
            p.set_len(h, 3 * BS).unwrap();
        } else {
            p.write_at(h, 3 * BS - 1, &[0]).unwrap();
        }
        assert!(
            read_range(&p, h, 0, want.len()) == want,
            "{cut} {regrow}: open handle"
        );
        p.close(h).unwrap();
        assert!(read_file(&p, "f") == want, "{cut} {regrow}: after close");
    }
}

/// A layer `l` with provider `lp` and a closed file `f` holding `body`;
/// returns the file's store id and GUID.
pub(super) fn layer_with_file(
    s: &Arc<Storage>,
    body: &[u8],
) -> (Arc<LayerProvider>, Arc<dyn Provider>, [u8; 17], [u8; 16]) {
    let lid = s.catalog.create_layer("l").unwrap();
    let lp: Arc<LayerProvider> = Arc::new(LayerProvider::new(Arc::clone(s), "l".into(), lid));
    let p: Arc<dyn Provider> = lp.clone();
    write_file(&p, "f", 0, body);
    let guid = s.catalog.get(lid, "f").unwrap().unwrap().guid;
    (lp, p, layer_file_id(&guid), guid)
}

/// What the block store holds for `id`: its whole length, with no block
/// missing.
pub(super) fn stored(s: &Storage, id: &[u8; 17]) -> Vec<u8> {
    let len = s.store.stat(id).unwrap().expect("store file").len;
    let mut buf = vec![0u8; len as usize];
    let r = s.store.read(id, 0, &mut buf).unwrap();
    assert!(
        r.missing.is_empty(),
        "store blocks missing: {:?}",
        r.missing
    );
    assert_eq!(r.bytes, buf.len());
    buf
}

/// A grow whose block writes fail after the store was resized (disk
/// full) puts the store back: the closed file's length and its tail
/// block are what they were, so a crash right after loses nothing that
/// was closed. The dirty block below the resize went in first.
#[test]
fn a_failed_grow_commit_keeps_the_closed_tail() {
    let (s, d) = temp_storage_every_close();
    let old: Vec<u8> = (0..(2 * BS + 100)).map(|i| (i % 251) as u8).collect();
    let (lp, p, id, guid) = layer_with_file(&s, &old);

    let (h, _, _) = p.open(at("f"), OPEN_WRITE).unwrap();
    p.write_at(h, 10, b"head").unwrap(); // block 0: below the resize
    p.write_at(h, 5 * BS, b"grown").unwrap();
    lp.live_cell(&guid)
        .unwrap()
        .fail_after_set_len
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(p.flush(h), Err(ST_IO_ERROR));

    let mut want_store = old.clone();
    want_store[10..14].copy_from_slice(b"head");
    assert_eq!(
        stored(&s, &id),
        want_store,
        "the store is back at the closed length"
    );
    let lid = s.catalog.layer_id("l").unwrap().unwrap();
    assert_eq!(
        s.catalog.get(lid, "f").unwrap().unwrap().len,
        old.len() as u64
    );

    // Killed now, after the store reached disk: the closed bytes are there.
    #[cfg(not(windows))]
    {
        s.store.flush().unwrap();
        let killed = vfs_testkit::tempdir().unwrap();
        snapshot_as_killed(d.path(), killed.path()).unwrap();
        let k = Storage::open(killed.path(), cfg()).unwrap();
        let r = k.last_reconcile();
        assert!(
            r.zero_filled_files.is_empty() && r.corrupt_files.is_empty(),
            "{r:?}"
        );
        let kp = k.layer("l").unwrap();
        assert_eq!(read_file(&kp, "f"), want_store);
    }
    #[cfg(windows)]
    let _ = d;

    // The handle still has its writes, and the next commit lands them.
    let mut want = want_store.clone();
    want.resize(5 * BS as usize, 0);
    want.extend_from_slice(b"grown");
    assert!(read_range(&p, h, 0, want.len() + 1) == want, "open handle");
    p.close(h).unwrap();
    assert!(read_file(&p, "f") == want, "after close");
}

/// A shrink to an unaligned length whose tail write fails after the
/// resize puts every dropped block back.
#[test]
fn a_failed_shrink_commit_keeps_the_closed_bytes() {
    let (s, _d) = temp_storage();
    let old: Vec<u8> = (0..(5 * BS + 100)).map(|i| (i % 253) as u8).collect();
    let (lp, p, id, guid) = layer_with_file(&s, &old);

    let (h, _, _) = p.open(at("f"), OPEN_WRITE).unwrap();
    lp.live_cell(&guid)
        .unwrap()
        .fail_after_set_len
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(p.set_len(h, 2 * BS + 7), Err(ST_IO_ERROR));
    assert_eq!(stored(&s, &id), old);

    p.close(h).unwrap();
    assert!(
        read_file(&p, "f") == old[..(2 * BS + 7) as usize],
        "after close"
    );
}

/// A shrink that drops more blocks than a commit captures first shrinks
/// the store to the block boundary above the new length (only bytes the
/// handle already cut go); a failure after that restores the tail block,
/// and the row follows the store's length.
#[test]
fn a_failed_large_shrink_commit_stops_at_the_block_boundary() {
    let (s, _d) = temp_storage();
    let old: Vec<u8> = (0..((crate::layer_io::MAX_CAPTURE_BLOCKS + 10) * BS + 100))
        .map(|i| (i % 249) as u8)
        .collect();
    let (lp, p, id, guid) = layer_with_file(&s, &old);

    let (h, _, _) = p.open(at("f"), OPEN_WRITE).unwrap();
    lp.live_cell(&guid)
        .unwrap()
        .fail_after_set_len
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(p.set_len(h, 100), Err(ST_IO_ERROR));
    assert_eq!(stored(&s, &id), old[..BS as usize]);
    let lid = s.catalog.layer_id("l").unwrap().unwrap();
    assert_eq!(s.catalog.get(lid, "f").unwrap().unwrap().len, BS);
    assert_eq!(p.getattr(at("f")).unwrap().unwrap().size, 100);

    p.close(h).unwrap();
    assert!(read_file(&p, "f") == old[..100], "after close");
    assert_eq!(s.catalog.get(lid, "f").unwrap().unwrap().len, 100);
}

#[test]
fn put_files_failing_after_its_rows_keeps_their_data() {
    let (s, d) = temp_storage();
    let id = s.catalog.create_layer("l").unwrap();
    let lp = LayerProvider::new(Arc::clone(&s), "l".into(), id);
    let big: Vec<u8> = (0..2 * BS as usize + 3).map(|i| (i % 249) as u8).collect();
    lp.fail_after_rows
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        lp.put_files(&[("c/a", b"alpha"), ("c/big", &big)]),
        Err(ST_IO_ERROR)
    );
    // The rows were committed: their data is still there.
    let p: Arc<dyn Provider> = Arc::new(lp);
    assert_eq!(read_file(&p, "c/a"), b"alpha");
    assert_eq!(read_file(&p, "c/big"), big);
    drop(p);
    s.close().unwrap();
    let s = Storage::open(d.path(), cfg()).unwrap();
    let p = s.layer("l").unwrap();
    assert_eq!(read_file(&p, "c/a"), b"alpha");
    assert_eq!(read_file(&p, "c/big"), big);
    assert!(s.store.verify().unwrap().is_ok());
}

#[test]
fn a_failed_create_rolls_back_its_parent_directories() {
    let (s, _d) = temp_storage();
    let id = s.catalog.create_layer("l").unwrap();
    let lp = LayerProvider::new(Arc::clone(&s), "l".into(), id);
    lp.mkdir(at("keep")).unwrap();
    lp.fail_store_create
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        lp.open(at("keep/new/deeper/f.txt"), OPEN_WRITE | OPEN_CREATE)
            .map(|_| ()),
        Err(ST_IO_ERROR)
    );
    assert!(lp.getattr(at("keep/new")).unwrap().is_none());
    assert!(lp.getattr(at("keep/new/deeper/f.txt")).unwrap().is_none());
    assert!(lp.getattr(at("keep")).unwrap().is_some());
}

#[test]
fn closed_file_is_durable_across_storage_reopen() {
    let (s, d) = temp_storage_every_close();
    let p = s.layer("saves").unwrap();
    write_file(&p, "Saves/one.ess", 0, b"saved game");
    // A second file still open (unflushed) must not stop the first from
    // being durable.
    let (h, _, _) = p
        .open(at("Saves/two.ess"), OPEN_WRITE | OPEN_CREATE)
        .unwrap();
    p.write_at(h, 0, b"in progress").unwrap();

    // Killed now: nothing but the close of one.ess has flushed anything.
    // Not on Windows, where redb and the store hold mandatory locks that
    // make a live copy fail.
    #[cfg(not(windows))]
    {
        let killed = vfs_testkit::tempdir().unwrap();
        snapshot_as_killed(d.path(), killed.path()).unwrap();
        let k = Storage::open(killed.path(), cfg()).unwrap();
        let kp = k.layer("saves").unwrap();
        assert_eq!(read_file(&kp, "saves/ONE.ess"), b"saved game");
    }

    // And the brief's variant: drop Storage without `close()`.
    p.close(h).unwrap();
    drop(p);
    drop(s);
    let s = Storage::open(d.path(), cfg()).unwrap();
    let p = s.layer("saves").unwrap();
    assert_eq!(read_file(&p, "saves/one.ess"), b"saved game");
    assert_eq!(read_file(&p, "saves/two.ess"), b"in progress");
}

/// The files a many-close test writes.
pub(super) const MANY: usize = 200;

/// Deferred: closes, flushes and namespace changes make no durable point.
/// `OnEveryClose`: each makes one.
#[test]
fn deferred_changes_make_no_durable_point_on_every_close_makes_one_each() {
    for (durability, per_op) in [(Durability::default(), 0), (Durability::OnEveryClose, 1)] {
        let (s, _d) = temp_storage_with(durability);
        let p = s.layer("l").unwrap();
        let before = s.clock.points();
        for i in 0..5 {
            write_file(&p, &format!("d/f{i}"), 0, b"body");
        }
        assert_eq!(
            s.clock.points() - before,
            5 * per_op,
            "{durability:?} closes"
        );
        let (h, _, _) = p.open(at("d/f0"), OPEN_WRITE).unwrap();
        p.write_at(h, 0, b"B").unwrap();
        p.flush(h).unwrap();
        assert_eq!(
            s.clock.points() - before,
            6 * per_op,
            "{durability:?} flush"
        );
        p.close(h).unwrap();
        p.mkdir(at("e")).unwrap();
        p.rename(at("d/f1"), at("e/f1")).unwrap();
        p.remove(at("d/f2")).unwrap();
        p.set_attr(
            at("d/f3"),
            SetAttr {
                size: Some(1),
                mtime: None,
            },
        )
        .unwrap();
        // (`OnEveryClose`: the close after the flush had nothing left to
        // make durable, so it skipped the fsyncs.)
        assert_eq!(
            s.clock.points() - before,
            10 * per_op,
            "{durability:?} namespace changes"
        );
        // A sync with changes pending is a durable point (under
        // `OnEveryClose` none are).
        s.sync().unwrap();
        assert_eq!(s.clock.points() - before, 10 * per_op + (1 - per_op));
    }
}

/// `sync` (which `close` runs) and a provider's drop skip the fsyncs
/// when nothing changed since the last durable point.
#[test]
fn an_idle_sync_drop_or_close_makes_no_durable_point() {
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    write_file(&p, "gone", 0, b"x");
    p.remove(at("gone")).unwrap();
    s.sync().unwrap();
    // The removed file's store delete ran after that commit, so one
    // more durable point has something to publish; after it, nothing.
    s.sync().unwrap();
    let settled = s.clock.points();
    let (h, _, _) = p.open(at(""), OPEN_READ).unwrap(); // reads change nothing
    p.close(h).unwrap();
    s.sync().unwrap();
    drop(p);
    assert_eq!(s.clock.points(), settled, "idle sync and drop");
    let q = s.layer("l").unwrap();
    write_file(&q, "f", 0, b"y");
    s.sync().unwrap();
    assert_eq!(s.clock.points(), settled + 1, "a change makes it count");
}

/// A storage whose layer "content" has the scratch directory `Tmp`.
pub(super) fn scratch_storage(d: &std::path::Path) -> Arc<Storage> {
    Storage::open(
        d,
        StorageConfig {
            scratch_dirs: vec![crate::ScratchDir {
                layer: "content".into(),
                dir: "Tmp".into(),
            }],
            ..cfg()
        },
    )
    .unwrap()
}

/// A file open across a durable point makes one at its close (its row
/// is durable now). In its layer's scratch directory it does not, so
/// many large temporaries written at once do not chain durable points;
/// the same directory in any other layer (a game's write layer) keeps
/// the rule, as does any other directory of the scratch layer.
#[test]
fn a_scratch_file_open_across_a_durable_point_makes_none_at_close() {
    let d = vfs_testkit::tempdir().unwrap();
    let s = scratch_storage(d.path());
    let c = s.layer("content").unwrap();
    let w = s.layer("write").unwrap();
    for (p, dir) in [(&c, "tmp"), (&c, "keep"), (&w, "tmp")] {
        p.mkdir(at(dir)).unwrap();
    }
    s.sync().unwrap();
    let (t, _, _) = c.open(at("TMP/a"), OPEN_WRITE | OPEN_CREATE).unwrap();
    let (k, _, _) = c.open(at("keep/a"), OPEN_WRITE | OPEN_CREATE).unwrap();
    let (g, _, _) = w.open(at("tmp/a"), OPEN_WRITE | OPEN_CREATE).unwrap();
    for (p, h, v) in [(&c, t, 1u8), (&c, k, 2), (&w, g, 5)] {
        p.write_at(h, 0, &[v; 3 * BS as usize]).unwrap();
    }
    s.sync().unwrap();
    for (p, h, v) in [(&c, t, 3u8), (&c, k, 4), (&w, g, 6)] {
        p.write_at(h, 3 * BS, &[v; BS as usize]).unwrap();
    }
    let before = s.clock.points();
    c.close(t).unwrap();
    assert_eq!(
        s.clock.points(),
        before,
        "the content layer's scratch file: deferred"
    );
    c.close(k).unwrap();
    assert_eq!(
        s.clock.points(),
        before + 1,
        "another directory of the content layer: a durable point"
    );
    w.close(g).unwrap();
    assert_eq!(
        s.clock.points(),
        before + 2,
        "tmp/ of another layer: a durable point"
    );
    // All read back whole, and do after a reopen.
    let mut buf = vec![0u8; 4 * BS as usize];
    let (h, _, _) = c.open(at("tmp/a"), OPEN_READ).unwrap();
    assert_eq!(c.read_at(h, 0, &mut buf).unwrap(), buf.len());
    assert_eq!(buf[3 * BS as usize], 3);
    c.close(h).unwrap();
    drop((c, w));
    drop(s);
    let s = Storage::open(d.path(), cfg()).unwrap();
    let c = s.layer("content").unwrap();
    let (h, _, _) = c.open(at("tmp/a"), OPEN_READ).unwrap();
    assert_eq!(c.read_at(h, 0, &mut buf).unwrap(), buf.len());
    assert_eq!((buf[0], buf[3 * BS as usize]), (1, 3));
    c.close(h).unwrap();
}

/// The hazard the rewrite rule guards against, for a scratch file: its
/// row is durable, the store alone is flushed in the middle of a
/// rewrite, it is closed (no durable point) and renamed onto its final
/// name, and the process is killed. The final name is absent or whole,
/// nothing outside the scratch directory is damaged, and the host can
/// clear the scratch directory. With a durable point after the rename,
/// the final name is whole.
#[cfg(not(windows))]
#[test]
fn a_killed_scratch_rewrite_leaves_its_final_name_absent_or_whole() {
    for sync_after_rename in [false, true] {
        let d = vfs_testkit::tempdir().unwrap();
        let s = scratch_storage(d.path());
        let c = s.layer("content").unwrap();
        c.mkdir(at("tmp")).unwrap();
        c.mkdir(at("c")).unwrap();
        write_file(&c, "c/old", 0, b"kept");
        let (h, _, _) = c.open(at("tmp/a"), OPEN_WRITE | OPEN_CREATE).unwrap();
        c.write_at(h, 0, &[1; 2 * BS as usize]).unwrap();
        s.sync().unwrap(); // tmp/a's row (two blocks) is durable now
        c.set_len(h, 0).unwrap();
        c.write_at(h, 0, &[2; 3 * BS as usize]).unwrap();
        c.flush(h).unwrap(); // committed, not durable (scratch)
        s.store.flush().unwrap(); // a store auto-flush mid-rewrite
        c.write_at(h, 3 * BS, &[3; BS as usize]).unwrap();
        c.close(h).unwrap();
        c.rename(at("tmp/a"), at("c/k")).unwrap();
        if sync_after_rename {
            s.sync().unwrap();
        }
        let (k, kp, _killed) = killed_copy(d.path(), "content");
        let mut want = vec![2u8; 3 * BS as usize];
        want.extend_from_slice(&[3; BS as usize]);
        match kp.getattr(at("c/k")).unwrap() {
            Some(_) => assert_eq!(read_file(&kp, "c/k"), want, "c/k is whole"),
            None => assert!(!sync_after_rename, "a synced rename is durable"),
        }
        assert_eq!(read_file(&kp, "c/old"), b"kept");
        // The host clears its scratch directory, whatever is in it.
        for n in names(&kp, "tmp") {
            kp.remove(at(&format!("tmp/{n}"))).unwrap();
        }
        assert!(names(&kp, "tmp").is_empty());
        k.sync().unwrap();
        assert!(k.store.verify().unwrap().is_ok());
    }
}

/// Once the last durable point is `max_interval` old, the next change
/// makes one, and the interval starts again.
#[test]
fn a_change_after_max_interval_makes_a_durable_point() {
    let max_interval = std::time::Duration::from_secs(60);
    let (s, _d) = temp_storage_with(Durability::Deferred { max_interval });
    let p = s.layer("l").unwrap();
    let before = s.clock.points();
    write_file(&p, "a", 0, b"a");
    s.clock.advance(max_interval / 2);
    write_file(&p, "b", 0, b"b");
    assert_eq!(s.clock.points(), before, "not yet due");
    s.clock.advance(max_interval / 2);
    write_file(&p, "c", 0, b"c");
    assert_eq!(s.clock.points(), before + 1, "due: the close made one");
    write_file(&p, "d", 0, b"d");
    assert_eq!(s.clock.points(), before + 1, "the interval restarted");
    s.clock.advance(max_interval);
    p.mkdir(at("x")).unwrap();
    assert_eq!(s.clock.points(), before + 2, "a namespace change too");
}

/// A due durable point run by a change deletes every live layer's
/// deferred deletions, not only its own layer's.
#[test]
fn a_due_durable_point_covers_every_live_layer() {
    let max_interval = std::time::Duration::from_secs(60);
    let (s, _d) = temp_storage_with(Durability::Deferred { max_interval });
    let a = s.layer("a").unwrap();
    let b = s.layer("b").unwrap();
    write_file(&b, "gone", 0, b"x");
    let lid = s.catalog.layer_id("b").unwrap().unwrap();
    let id = layer_file_id(&s.catalog.get(lid, "gone").unwrap().unwrap().guid);
    b.remove(at("gone")).unwrap();
    assert!(s.store.stat(&id).unwrap().is_some(), "deferred");
    s.clock.advance(max_interval);
    write_file(&a, "f", 0, b"y");
    assert!(s.store.stat(&id).unwrap().is_none(), "deleted by a's point");
}

/// Deferred: a removed or replaced file's store data stays until a
/// durable point has made the removal durable; `sync` deletes it.
#[test]
fn deferred_deletions_wait_for_a_durable_point() {
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    write_file(&p, "removed", 0, b"r");
    write_file(&p, "old", 0, b"o");
    write_file(&p, "new", 0, b"n");
    let lid = s.catalog.layer_id("l").unwrap().unwrap();
    let id_of = |path: &str| layer_file_id(&s.catalog.get(lid, path).unwrap().unwrap().guid);
    let removed = id_of("removed");
    let replaced = id_of("old");
    let (h, _, _) = p.open(at("new"), OPEN_READ).unwrap();
    p.remove(at("removed")).unwrap();
    p.rename(at("new"), at("old")).unwrap();
    p.close(h).unwrap();
    assert!(s.store.stat(&removed).unwrap().is_some(), "removed: kept");
    assert!(s.store.stat(&replaced).unwrap().is_some(), "replaced: kept");
    assert_eq!(read_file(&p, "old"), b"n");
    s.sync().unwrap();
    assert!(
        s.store.stat(&removed).unwrap().is_none(),
        "removed: deleted"
    );
    assert!(
        s.store.stat(&replaced).unwrap().is_none(),
        "replaced: deleted"
    );
    assert_eq!(read_file(&p, "old"), b"n");
}

/// No file was emptied, zero-filled, found corrupt or resized, and no
/// repair failed: only orphans (and cache rows) may have been dropped.
#[cfg(not(windows))]
pub(super) fn assert_no_file_repaired(r: &crate::ReconcileReport) {
    assert!(
        r.emptied_files.is_empty()
            && r.zero_filled_files.is_empty()
            && r.corrupt_files.is_empty()
            && r.resized_rows.is_empty()
            && r.failed_repairs.is_empty(),
        "{r:?}"
    );
}

/// Deferred, killed: what `sync` made durable is there; what came after
/// may be gone, but only whole — the store reopens consistent, with no
/// file emptied, zero-filled or corrupt, and a removal that was not yet
/// durable brings its file back with all its data.
#[cfg(not(windows))]
#[test]
fn deferred_crash_keeps_what_sync_made_durable_and_reopens_clean() {
    let (s, d) = temp_storage();
    let p = s.layer("l").unwrap();
    let big: Vec<u8> = (0..(3 * BS + 11)).map(|i| (i % 251) as u8).collect();
    write_file(&p, "Kept/big.bin", 0, &big);
    write_file(&p, "kept/doomed.txt", 0, b"removed later");
    s.sync().unwrap();
    write_file(&p, "later/new.bin", 0, &big);
    write_file(&p, "save.tmp", 0, b"new save");
    p.rename(at("save.tmp"), at("kept/save.ess")).unwrap();
    p.remove(at("kept/doomed.txt")).unwrap();
    // The store half of the later writes reached disk; the catalog's did not.
    s.store.flush().unwrap();

    let (k, kp, _kd) = killed_copy(d.path(), "l");
    let r = k.last_reconcile();
    assert_no_file_repaired(r);
    assert!(r.orphans_deleted >= 2, "the unpublished files' data: {r:?}");
    assert_eq!(names(&kp, ""), ["Kept"]);
    assert_eq!(names(&kp, "kept"), ["big.bin", "doomed.txt"]);
    assert_eq!(read_file(&kp, "kept/big.bin"), big);
    assert_eq!(read_file(&kp, "kept/doomed.txt"), b"removed later");
    drop(kp);
    k.close().unwrap();

    // The same, after a sync: everything is there. (The removed file's
    // store delete, made after the durable commit, is itself not durable
    // yet: reconciliation deletes that data as an orphan.)
    s.sync().unwrap();
    let (k, kp, _kd) = killed_copy(d.path(), "l");
    assert_no_file_repaired(k.last_reconcile());
    assert_eq!(names(&kp, "kept"), ["big.bin", "save.ess"]);
    assert_eq!(read_file(&kp, "kept/save.ess"), b"new save");
    assert_eq!(read_file(&kp, "later/new.bin"), big);
}

/// Deferred, dropped without `Storage::close`: the provider's drop is a
/// durable point, so nothing closed is lost, even if the process dies
/// right after it; and the storage's own drop is a clean close.
#[test]
fn deferred_writes_survive_a_drop_without_close() {
    let (s, d) = temp_storage();
    let p = s.layer("l").unwrap();
    write_file(&p, "a", 0, b"after no sync");
    drop(p);
    #[cfg(not(windows))]
    {
        let (k, kp, _kd) = killed_copy(d.path(), "l");
        assert_eq!(*k.last_reconcile(), Default::default());
        assert_eq!(read_file(&kp, "a"), b"after no sync");
    }
    drop(s);
    let s = Storage::open(d.path(), cfg()).unwrap();
    assert!(s.last_reconcile().skipped_after_clean_close);
    assert_eq!(read_file(&s.layer("l").unwrap(), "a"), b"after no sync");
}

/// Deferred: rewriting a file that is already durable, in place, with
/// the block store flushing in the middle (its auto-flush), then a crash
/// after the close: the file comes back whole (the close made a durable
/// point), never emptied or torn.
#[cfg(not(windows))]
#[test]
fn deferred_rewrite_of_a_durable_file_survives_a_mid_rewrite_store_flush() {
    let (s, d) = temp_storage();
    let lp = s.layer_provider("l", true).unwrap();
    let p: Arc<dyn Provider> = lp.clone();
    let old: Vec<u8> = (0..(3 * BS + 10)).map(|i| (i % 251) as u8).collect();
    let new: Vec<u8> = (0..(5 * BS + 3)).map(|i| (i % 13) as u8).collect();
    write_file(&p, "trunc.bin", 0, &old);
    write_file(&p, "grow.bin", 0, &old[..(BS + 10) as usize]);
    s.sync().unwrap();

    // Truncate on open (commits the store's resize to 0), store flush,
    // then the new content.
    let (h, _, _) = p.open(at("trunc.bin"), OPEN_WRITE | OPEN_TRUNC).unwrap();
    s.store.flush().unwrap();
    p.write_at(h, 0, &new).unwrap();
    p.close(h).unwrap();

    // Grow: the close's commit resizes the store, which flushes before
    // the block writes.
    let (h, _, _) = p.open(at("grow.bin"), OPEN_WRITE).unwrap();
    p.write_at(h, 0, &new).unwrap();
    let lid = s.catalog.layer_id("l").unwrap().unwrap();
    let guid = s.catalog.get(lid, "grow.bin").unwrap().unwrap().guid;
    lp.live_cell(&guid)
        .unwrap()
        .flush_after_set_len
        .store(true, std::sync::atomic::Ordering::SeqCst);
    p.close(h).unwrap();

    let (k, kp, _kd) = killed_copy(d.path(), "l");
    assert_no_file_repaired(k.last_reconcile());
    for f in ["trunc.bin", "grow.bin"] {
        let got = read_file(&kp, f);
        assert!(got == new, "{f}: {} bytes, not the new content", got.len());
    }
}

/// Deferred: a new file renamed over a durable one, the store flushed,
/// then a crash: the durable file is there, old or new, and whole.
#[cfg(not(windows))]
#[test]
fn deferred_rename_over_a_durable_file_then_a_crash_keeps_a_whole_file() {
    let (s, d) = temp_storage();
    let p = s.layer("l").unwrap();
    let old = vec![0x11u8; 2 * BS as usize + 5];
    let new = vec![0x22u8; 3 * BS as usize + 7];
    write_file(&p, "save.ess", 0, &old);
    s.sync().unwrap();
    write_file(&p, "save.tmp", 0, &new);
    p.rename(at("save.tmp"), at("save.ess")).unwrap();
    s.store.flush().unwrap();

    let (k, kp, _kd) = killed_copy(d.path(), "l");
    assert_no_file_repaired(k.last_reconcile());
    assert_eq!(names(&kp, ""), ["save.ess"]);
    let got = read_file(&kp, "save.ess");
    assert!(got == old || got == new, "{} bytes", got.len());
}

/// Deferred: a durable point is also due once the catalog holds
/// `max_commits` non-durable commits, whatever `max_interval` says.
#[test]
fn deferred_changes_make_a_durable_point_at_the_commit_bound() {
    let (s, _d) = temp_storage();
    s.clock.set_max_commits(20);
    let p = s.layer("l").unwrap();
    let before = s.clock.points();
    for i in 0..50 {
        write_file(&p, &format!("f{i}"), 0, b"x");
    }
    let made = s.clock.points() - before;
    assert!(
        (1..50).contains(&made),
        "{made} durable points for 50 closes"
    );
    assert!(s.catalog.unflushed_commits() < 20 + 10);
}

/// `sync` never holds a layer's provider: a dropped provider's layer
/// can be deleted at once while other threads sync.
#[test]
fn delete_layer_after_drop_is_not_refused_while_syncing() {
    let (s, _d) = temp_storage();
    let other = s.layer("other").unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let syncer = {
        let (s, other, stop) = (Arc::clone(&s), Arc::clone(&other), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut i = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Acquire) {
                write_file(&other, "o", 0, &i.to_le_bytes());
                s.sync().unwrap();
                i += 1;
            }
        })
    };
    for i in 0..100 {
        let p = s.layer("x").unwrap();
        write_file(&p, "f", 0, b"x");
        drop(p);
        if let Err(e) = s.delete_layer("x") {
            stop.store(true, std::sync::atomic::Ordering::Release);
            syncer.join().unwrap();
            panic!("iteration {i}: {e}");
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Release);
    syncer.join().unwrap();
    drop(other);
}

/// Deferred: a file removed while open keeps its store data past its
/// last close, until a durable point.
#[test]
fn deferred_remove_while_open_deletes_the_data_at_the_durable_point() {
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    write_file(&p, "gone.txt", 0, b"still readable");
    let lid = s.catalog.layer_id("l").unwrap().unwrap();
    let id = layer_file_id(&s.catalog.get(lid, "gone.txt").unwrap().unwrap().guid);
    let (h, _, _) = p.open(at("gone.txt"), OPEN_READ).unwrap();
    p.remove(at("gone.txt")).unwrap();
    assert!(p.getattr(at("gone.txt")).unwrap().is_none());
    assert_eq!(read_range(&p, h, 0, 14), b"still readable");
    p.close(h).unwrap();
    assert!(s.store.stat(&id).unwrap().is_some(), "deferred past close");
    s.sync().unwrap();
    assert!(s.store.stat(&id).unwrap().is_none(), "deleted by sync");
}

/// Many deferred closes: no durable point at all, and cheap.
#[test]
fn many_deferred_closes_make_no_durable_point() {
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    let before = s.clock.points();
    let t = std::time::Instant::now();
    for i in 0..MANY {
        write_file(&p, &format!("d{}/f{i}.txt", i % 10), 0, b"small file");
    }
    let took = t.elapsed();
    assert_eq!(s.clock.points(), before);
    eprintln!("{MANY} deferred closes: {took:?}");
    s.sync().unwrap();
    assert_eq!(s.clock.points(), before + 1);
}

/// Timing comparison of the two policies (fsync cost depends on the
/// filesystem, so it only reports): `cargo test -p vfs-storage --
/// --ignored --nocapture deferred_vs_every_close`.
#[test]
#[ignore]
fn deferred_vs_every_close_timing() {
    for durability in [Durability::default(), Durability::OnEveryClose] {
        let (s, _d) = temp_storage_with(durability);
        let p = s.layer("l").unwrap();
        let t = std::time::Instant::now();
        for i in 0..MANY {
            write_file(&p, &format!("d{}/f{i}.txt", i % 10), 0, b"small file");
        }
        s.sync().unwrap();
        eprintln!("{durability:?}: {MANY} closes + sync: {:?}", t.elapsed());
    }
}
