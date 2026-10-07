use super::*;

#[test]
fn write_opcode_round_trips_through_dispatch() {
    use vfs_protocol::{
        decode_open_resp, decode_write_resp, encode_open_req, encode_write_req, WriteReq,
        OPEN_CREATE, OPEN_WRITE, OP_OPEN, OP_WRITE, ST_OK,
    };
    let dir = vfs_testkit::scratch_path("vfs-rdw");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let d = Director::new();
    d.mount(
        RootId::DEFAULT,
        std::sync::Arc::new(vfs_compose::DiskProvider::new(&dir)),
    )
    .unwrap();

    let (st, payload) = dispatch_director(
        &d,
        OP_OPEN,
        &encode_open_req(0, OPEN_WRITE | OPEN_CREATE, "w.txt"),
        0,
        4096,
        None,
    );
    assert_eq!(st, ST_OK, "open for write must succeed through dispatch");
    let fh = decode_open_resp(&payload).unwrap().fh;

    let req = WriteReq {
        fh,
        offset: 0,
        len: 5,
    };
    let (st, payload) = dispatch_director(
        &d,
        OP_WRITE,
        &encode_write_req(&req, b"hello"),
        0,
        4096,
        None,
    );
    assert_eq!(st, ST_OK, "write must succeed through dispatch");
    assert_eq!(decode_write_resp(&payload).unwrap(), 5);

    assert_eq!(std::fs::read(dir.join("w.txt")).unwrap(), b"hello");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `OP_STORED_NAMES`: the stored spelling of a path's components in one
/// round trip, from the `skip`-th on, with a component nothing has
/// answered as it was asked.
#[test]
fn stored_names_opcode_answers_the_spelling_of_each_component() {
    use vfs_protocol::{encode_names_req, OP_STORED_NAMES, ST_OK};
    let d = Director::new();
    d.mount(
        RootId::DEFAULT,
        std::sync::Arc::new(vfs_compose::InlineProvider::from_files([(
            "Data/Interface/Fonts/Jost-Regular.ttf",
            b"x".as_slice(),
        )])),
    )
    .unwrap();
    let ask = |skip: u32, path: &str| {
        let (st, payload) = dispatch_director(
            &d,
            OP_STORED_NAMES,
            &encode_names_req(0, skip, path),
            0,
            4096,
            None,
        );
        assert_eq!(st, ST_OK, "{path}");
        String::from_utf8(payload).unwrap()
    };
    assert_eq!(
        ask(0, "data/interface/fonts/jost-regular.ttf"),
        "Data/Interface/Fonts/Jost-Regular.ttf"
    );
    assert_eq!(
        ask(2, "DATA/INTERFACE/FONTS/JOST-REGULAR.TTF"),
        "Fonts/Jost-Regular.ttf"
    );
    assert_eq!(ask(4, "data/interface/fonts/jost-regular.ttf"), "");
    // What nothing has keeps the caller's spelling, and what is above it
    // is still the stored one.
    assert_eq!(
        ask(0, "data/INTERFACE/New Dir/New.TXT"),
        "Data/Interface/New Dir/New.TXT"
    );
    assert_eq!(ask(0, ""), "");
}

#[test]
fn delete_opcode_removes_the_file() {
    use vfs_protocol::{encode_path_req, OP_DELETE, ST_OK};
    let dir = vfs_testkit::scratch_path("vfs-rdd");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("gone.txt"), b"x").unwrap();
    let d = Director::new();
    d.mount(
        RootId::DEFAULT,
        std::sync::Arc::new(vfs_compose::DiskProvider::new(&dir)),
    )
    .unwrap();

    let (st, _) = dispatch_director(
        &d,
        OP_DELETE,
        &encode_path_req(0, "gone.txt"),
        0,
        4096,
        None,
    );
    assert_eq!(st, ST_OK);
    assert!(
        !dir.join("gone.txt").exists(),
        "OP_DELETE did not remove the file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_write_against_a_read_only_provider_is_read_only_not_bad_request() {
    use vfs_protocol::{encode_open_req, OPEN_WRITE, OP_OPEN, ST_READ_ONLY};
    let d = Director::new();
    // InlineProvider is Access::Read: the mount itself has no writable
    // backend, so the director (not this dispatch arm) must be the one
    // to say so.
    d.mount(
        RootId::DEFAULT,
        std::sync::Arc::new(vfs_compose::InlineProvider::from_files([(
            "f",
            b"x".as_slice(),
        )])),
    )
    .unwrap();

    let (st, _) = dispatch_director(
        &d,
        OP_OPEN,
        &encode_open_req(0, OPEN_WRITE, "f"),
        0,
        4096,
        None,
    );
    assert_eq!(
        st, ST_READ_ONLY,
        "OP_OPEN with OPEN_WRITE against a read-only mount must surface ST_READ_ONLY, not a blanket ST_BAD_REQUEST"
    );
}

/// Stage 2b task 3, step 1: `[0, "a.txt"]` and `[1, "a.txt"]` resolve to
/// different content through the director, end to end via
/// `dispatch_director` — the ring-level counterpart to
/// `director::tests::two_roots_resolve_the_same_relative_path_independently`.
/// Updated by task 5: the root is no longer a Rust-level parameter this
/// test had to supply out of band — it now rides in the OPEN payload
/// itself, so `encode_open_req(root, …)` is what selects the provider and
/// `dispatch_director` takes no root argument at all. The subsequent READ
/// carries none, and needs none: the handle OPEN returned already knows
/// which root it came from.
#[test]
fn different_roots_resolve_the_same_path_to_different_content_via_dispatch() {
    use vfs_protocol::{
        decode_open_resp, decode_read_resp, encode_open_req, encode_read_req, ReadReq, OPEN_READ,
        OP_OPEN, OP_READ, ST_OK,
    };
    let d = Director::new();
    d.mount(
        RootId(0),
        std::sync::Arc::new(vfs_compose::InlineProvider::from_files([(
            "a.txt",
            b"ROOT-ZERO".as_slice(),
        )])),
    )
    .unwrap();
    d.mount(
        RootId(1),
        std::sync::Arc::new(vfs_compose::InlineProvider::from_files([(
            "a.txt",
            b"ROOT-ONE".as_slice(),
        )])),
    )
    .unwrap();

    let read_via = |root: u32| -> Vec<u8> {
        let (st, payload) = dispatch_director(
            &d,
            OP_OPEN,
            &encode_open_req(root, OPEN_READ, "a.txt"),
            0,
            4096,
            None,
        );
        assert_eq!(st, ST_OK);
        let fh = decode_open_resp(&payload).unwrap().fh;
        let (st, payload) = dispatch_director(
            &d,
            OP_READ,
            &encode_read_req(&ReadReq {
                fh,
                offset: 0,
                len: 64,
            }),
            0,
            4096,
            None,
        );
        assert_eq!(st, ST_OK);
        decode_read_resp(&payload).unwrap()
    };

    assert_eq!(read_via(0), b"ROOT-ZERO");
    assert_eq!(read_via(1), b"ROOT-ONE");
}

/// Stage 2b task 5: the wire itself, not a caller-side parameter, is what
/// selects the root now. Two OPEN payloads differing **only** in their
/// leading `root:u32` must reach different providers — which is the whole
/// end-to-end claim, since `dispatch_director` has no other way left to
/// learn the root.
///
/// Also pins the failure mode a stale shim would produce: the pre-task-5
/// OPEN payload was `flags|path`, so feeding those bytes here reads the
/// flags as a root id and the first four path bytes as flags. That is
/// caught at ring attach by `vfs_ipc::layout::VERSION`, never here —
/// asserted below only so the reason the version bump is load-bearing is
/// recorded next to the code it protects.
#[test]
fn the_wire_root_alone_selects_the_provider() {
    use vfs_protocol::{
        decode_open_resp, decode_read_resp, encode_open_req, encode_read_req, ReadReq, OPEN_READ,
        OP_OPEN, OP_READ, ST_OK,
    };
    let d = Director::new();
    for (root, bytes) in [(0u32, b"ZERO".as_slice()), (1u32, b"ONE!".as_slice())] {
        d.mount(
            RootId(root),
            std::sync::Arc::new(vfs_compose::InlineProvider::from_files([("a.txt", bytes)])),
        )
        .unwrap();
    }

    let zero = encode_open_req(0, OPEN_READ, "a.txt");
    let one = encode_open_req(1, OPEN_READ, "a.txt");
    assert_ne!(
        zero, one,
        "the two payloads must differ only in the root field"
    );
    assert_eq!(&zero[4..], &one[4..], "…and in nothing else");

    let read = |payload: &[u8]| -> Vec<u8> {
        let (st, resp) = dispatch_director(&d, OP_OPEN, payload, 0, 4096, None);
        assert_eq!(st, ST_OK);
        let fh = decode_open_resp(&resp).unwrap().fh;
        let (st, resp) = dispatch_director(
            &d,
            OP_READ,
            &encode_read_req(&ReadReq {
                fh,
                offset: 0,
                len: 64,
            }),
            0,
            4096,
            None,
        );
        assert_eq!(st, ST_OK);
        decode_read_resp(&resp).unwrap()
    };
    assert_eq!(read(&zero), b"ZERO");
    assert_eq!(read(&one), b"ONE!");

    // The stale-shim shape: `flags:u32 | path`, i.e. the new encoding with
    // the root field missing. It does not resolve to anything sensible —
    // which is exactly why the ring refuses to attach such a shim rather
    // than letting it reach here.
    let mut stale = OPEN_READ.to_le_bytes().to_vec();
    stale.extend_from_slice(b"a.txt");
    let (st, _) = dispatch_director(&d, OP_OPEN, &stale, 0, 4096, None);
    assert_ne!(
        st, ST_OK,
        "a pre-task-5 OPEN payload must not silently succeed"
    );
}

// ---- registry overlay opcodes (15-22) ----

mod registry_ops {
    use super::super::*;
    use crate::registry::RegistryHost;
    use std::sync::Arc;
    use vfs_protocol::*;
    use vfs_registry::Child;

    const K: &str = r"\Registry\Machine\Software\Mod";
    const CAP: u32 = 1 << 20;

    fn director() -> Director {
        let d = Director::new();
        let host = RegistryHost::open(Arc::new(vfs_provider::RwMemFixture::new())).unwrap();
        d.set_registry(Some(host));
        d
    }

    fn call(d: &Director, op: u32, payload: &[u8]) -> (i32, Vec<u8>) {
        dispatch_director(d, op, payload, 0, CAP, None)
    }

    fn ok(d: &Director, op: u32, payload: &[u8]) -> Vec<u8> {
        let (st, r) = call(d, op, payload);
        assert_eq!(st, ST_OK, "op {op}");
        r
    }

    fn version(d: &Director, op: u32, payload: &[u8]) -> u64 {
        decode_reg_version_reply(&ok(d, op, payload)).unwrap()
    }

    /// A sink that records what was published to it, as a ring header would hold it.
    #[derive(Default)]
    struct Published(std::sync::atomic::AtomicU64);

    impl crate::RegistryGenSink for Published {
        fn publish_reg_generation(&self, generation: u64) {
            self.0
                .fetch_max(generation, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl Published {
        fn get(&self) -> u64 {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    fn sink(d: &Director) -> Arc<Published> {
        let p = Arc::new(Published::default());
        let weak: std::sync::Weak<dyn crate::RegistryGenSink> =
            Arc::downgrade(&(p.clone() as Arc<dyn crate::RegistryGenSink>));
        d.add_registry_sink(weak);
        p
    }

    /// Every successful registry write publishes a new generation before its reply is
    /// returned; reads and refused writes publish nothing; attaching and detaching a layer
    /// publish too. This is what the shim's cache relies on across processes.
    #[test]
    fn a_write_publishes_a_new_generation_before_it_replies() {
        let d = Director::new();
        let p = sink(&d);
        let g0 = p.get();
        assert_ne!(g0, 0, "a sink gets the current generation when added");
        assert_eq!(g0, d.registry_generation());

        let host = RegistryHost::open(Arc::new(vfs_provider::RwMemFixture::new())).unwrap();
        d.set_registry(Some(host));
        let g1 = p.get();
        assert!(g1 > g0, "attaching a layer publishes");

        // Reads do not move it.
        ok(&d, OP_REG_LOOKUP, &encode_reg_path(K));
        ok(&d, OP_REG_KEY, &encode_reg_path(K));
        ok(&d, OP_REG_CHANGED, &encode_reg_changed(K, true, 0));
        assert_eq!(p.get(), g1);

        // Each kind of write moves it, and it is visible as soon as dispatch returns.
        let sub = format!(r"{K}\Sub");
        let writes: Vec<(u32, Vec<u8>)> = vec![
            (
                OP_REG_SET_VALUE,
                encode_reg_set_value(K, "v", 4, &1u32.to_le_bytes()),
            ),
            (OP_REG_DELETE_VALUE, encode_reg_delete_value(K, "v")),
            (OP_REG_CREATE_KEY, encode_reg_create_key(&sub, false)),
            (OP_REG_RENAME_KEY, encode_reg_rename_key(&sub, "Moved")),
            (OP_REG_DELETE_KEY, encode_reg_path(&format!(r"{K}\Moved"))),
        ];
        let mut last = g1;
        for (op, payload) in writes {
            ok(&d, op, &payload);
            let now = p.get();
            assert_eq!(now, last + 1, "op {op} must publish exactly once");
            assert_eq!(now, d.registry_generation());
            last = now;
        }

        // A refused write changes nothing and publishes nothing.
        assert_eq!(
            call(&d, OP_REG_CREATE_KEY, &encode_reg_create_key(K, false)).0,
            ST_EXISTS
        );
        assert_eq!(
            call(
                &d,
                OP_REG_SET_VALUE,
                &encode_reg_set_value("bad", "v", 4, &[])
            )
            .0,
            ST_BAD_REQUEST
        );
        assert_eq!(p.get(), last);

        // Detaching publishes: an answer cached under the layer is stale without it.
        d.set_registry(None);
        assert!(p.get() > last);
    }

    /// The host publishes, not the ring: a write made on the host directly (a host-side
    /// writer that never goes through `dispatch_director`) moves the generation too.
    #[test]
    fn a_host_write_outside_the_ring_publishes() {
        let d = director();
        let p = sink(&d);
        let g = p.get();
        let host = d.registry().unwrap();
        host.set_value(K, "v", 4, &[1, 0, 0, 0]).unwrap();
        assert_eq!(p.get(), g + 1);
        host.create_key(&format!(r"{K}\Sub"), false).unwrap();
        assert_eq!(p.get(), g + 2);
        // A refused write publishes nothing.
        assert!(host.create_key(&format!(r"{K}\Sub"), false).is_err());
        assert_eq!(p.get(), g + 2);
    }

    #[test]
    fn no_registry_attached_is_not_supported() {
        let d = Director::new();
        assert!(d.registry().is_none());
        for op in [
            OP_REG_LOOKUP,
            OP_REG_KEY,
            OP_REG_SET_VALUE,
            OP_REG_DELETE_VALUE,
            OP_REG_CREATE_KEY,
            OP_REG_DELETE_KEY,
            OP_REG_RENAME_KEY,
            OP_REG_CHANGED,
        ] {
            assert_eq!(
                call(&d, op, &encode_reg_path(K)).0,
                ST_NOT_SUPPORTED,
                "op {op}"
            );
        }
        // Attach then detach.
        let d = director();
        assert!(d.registry().is_some());
        d.set_registry(None);
        assert_eq!(
            call(&d, OP_REG_LOOKUP, &encode_reg_path(K)).0,
            ST_NOT_SUPPORTED
        );
    }

    #[test]
    fn every_opcode_round_trips() {
        let d = director();
        // LOOKUP on nothing.
        let r = ok(&d, OP_REG_LOOKUP, &encode_reg_path(K));
        assert_eq!(decode_reg_lookup_reply(&r), Some((0, false, 0)));
        // KEY on nothing.
        let r = ok(&d, OP_REG_KEY, &encode_reg_path(K));
        assert_eq!(decode_reg_key_reply(&r), Some((None, 0)));

        // SET_VALUE.
        assert_eq!(
            version(
                &d,
                OP_REG_SET_VALUE,
                &encode_reg_set_value(K, "Val", 4, &7u32.to_le_bytes())
            ),
            1
        );
        let r = ok(&d, OP_REG_LOOKUP, &encode_reg_path(K));
        assert_eq!(
            decode_reg_lookup_reply(&r),
            Some((1, false, 1)),
            "present, not created"
        );
        let r = ok(&d, OP_REG_LOOKUP, &encode_reg_path(r"\Registry\Machine"));
        assert_eq!(
            decode_reg_lookup_reply(&r),
            Some((1, true, 1)),
            "something below"
        );
        let (node, v) = decode_reg_key_reply(&ok(&d, OP_REG_KEY, &encode_reg_path(K))).unwrap();
        assert_eq!(v, 1);
        let node = node.unwrap();
        assert_eq!(node.values[0].name, "Val");
        assert_eq!(node.values[0].ty, 4);
        assert_eq!(node.values[0].data, 7u32.to_le_bytes());
        assert!(node.last_write > 0, "FILETIME stamped");

        // DELETE_VALUE.
        assert_eq!(
            version(&d, OP_REG_DELETE_VALUE, &encode_reg_delete_value(K, "VAL")),
            2
        );
        let (node, _) = decode_reg_key_reply(&ok(&d, OP_REG_KEY, &encode_reg_path(K))).unwrap();
        let node = node.unwrap();
        assert!(node.values.is_empty());
        assert_eq!(node.value_tombstones, vec!["val".to_string()]);

        // CREATE_KEY: created here (the shim only creates what does not exist for real).
        let sub = format!(r"{K}\Sub");
        assert_eq!(
            version(&d, OP_REG_CREATE_KEY, &encode_reg_create_key(&sub, false)),
            3
        );
        let r = ok(&d, OP_REG_LOOKUP, &encode_reg_path(&sub));
        assert_eq!(decode_reg_lookup_reply(&r), Some((2, false, 3)));
        assert_eq!(
            call(&d, OP_REG_CREATE_KEY, &encode_reg_create_key(&sub, false)).0,
            ST_EXISTS
        );
        let vol = format!(r"{K}\Vol");
        version(&d, OP_REG_CREATE_KEY, &encode_reg_create_key(&vol, true));
        let (n, _) = decode_reg_key_reply(&ok(&d, OP_REG_KEY, &encode_reg_path(&vol))).unwrap();
        assert!(n.unwrap().volatile);

        // RENAME_KEY.
        let v = version(&d, OP_REG_RENAME_KEY, &encode_reg_rename_key(&sub, "Moved"));
        assert_eq!(v, 5);
        let r = ok(&d, OP_REG_LOOKUP, &encode_reg_path(&sub));
        assert_eq!(
            decode_reg_lookup_reply(&r),
            Some((3, false, 5)),
            "old name tombstoned"
        );
        let (n, _) = decode_reg_key_reply(&ok(&d, OP_REG_KEY, &encode_reg_path(K))).unwrap();
        assert_eq!(
            n.unwrap().children["moved"],
            ("Moved".to_string(), Child::Present)
        );
        assert_eq!(
            call(
                &d,
                OP_REG_RENAME_KEY,
                &encode_reg_rename_key(&format!(r"{K}\Nope"), "X")
            )
            .0,
            ST_NOT_FOUND
        );

        // DELETE_KEY.
        let moved = format!(r"{K}\Moved");
        assert_eq!(version(&d, OP_REG_DELETE_KEY, &encode_reg_path(&moved)), 6);
        let r = ok(&d, OP_REG_LOOKUP, &encode_reg_path(&moved));
        assert_eq!(decode_reg_lookup_reply(&r), Some((3, false, 6)));
        assert_eq!(
            call(&d, OP_REG_DELETE_KEY, &encode_reg_path(&moved)).0,
            ST_NOT_FOUND
        );
        assert_eq!(
            call(&d, OP_REG_DELETE_KEY, &encode_reg_path(r"\Registry")).0,
            ST_BAD_REQUEST,
            "the root cannot be deleted"
        );

        // CHANGED.
        let r = ok(&d, OP_REG_CHANGED, &encode_reg_changed(K, false, 5));
        assert_eq!(decode_reg_changed_reply(&r), Some((true, 6)));
        let r = ok(&d, OP_REG_CHANGED, &encode_reg_changed(K, false, 6));
        assert_eq!(decode_reg_changed_reply(&r), Some((false, 6)));
    }

    #[test]
    fn changed_reports_subtree_and_version() {
        let d = director();
        let deep = format!(r"{K}\A\B");
        let v1 = version(
            &d,
            OP_REG_SET_VALUE,
            &encode_reg_set_value(&deep, "x", 1, b""),
        );
        let v2 = version(
            &d,
            OP_REG_SET_VALUE,
            &encode_reg_set_value(&deep, "y", 1, b""),
        );
        let ask = |p: &str, sub: bool, since: u64| {
            decode_reg_changed_reply(&ok(&d, OP_REG_CHANGED, &encode_reg_changed(p, sub, since)))
                .unwrap()
        };
        assert_eq!(ask(K, false, v1), (false, v2), "only below K");
        assert_eq!(ask(K, true, v1), (true, v2));
        assert_eq!(ask(&deep, false, v1), (true, v2));
        assert_eq!(ask(&deep, false, v2), (false, v2));
        assert_eq!(ask(r"\Registry\Machine\Elsewhere", true, 0), (false, v2));
        assert_eq!(
            call(&d, OP_REG_CHANGED, &encode_reg_changed("nope", true, 0)).0,
            ST_BAD_REQUEST
        );
    }

    #[test]
    fn malformed_payloads_and_bad_paths_are_bad_request() {
        let d = director();
        for op in OP_REG_LOOKUP..=OP_REG_CHANGED {
            assert_eq!(call(&d, op, &[1, 2, 3]).0, ST_BAD_REQUEST, "op {op}");
        }
        for bad in [
            "",
            r"\Device\Foo",
            r"\Registry\Machine\\X",
            r"\Registry\Machine\..\X",
        ] {
            assert_eq!(
                call(&d, OP_REG_LOOKUP, &encode_reg_path(bad)).0,
                ST_BAD_REQUEST
            );
            assert_eq!(
                call(&d, OP_REG_KEY, &encode_reg_path(bad)).0,
                ST_BAD_REQUEST
            );
            assert_eq!(
                call(
                    &d,
                    OP_REG_SET_VALUE,
                    &encode_reg_set_value(bad, "v", 1, b"")
                )
                .0,
                ST_BAD_REQUEST
            );
            assert_eq!(
                call(&d, OP_REG_CREATE_KEY, &encode_reg_create_key(bad, false)).0,
                ST_BAD_REQUEST
            );
        }
        // Name too long (key component > 255 UTF-16 units).
        let long = format!(r"{K}\{}", "k".repeat(256));
        assert_eq!(
            call(&d, OP_REG_CREATE_KEY, &encode_reg_create_key(&long, false)).0,
            ST_BAD_REQUEST
        );
        // Nothing above bumped the version.
        let r = ok(&d, OP_REG_LOOKUP, &encode_reg_path(K));
        assert_eq!(decode_reg_lookup_reply(&r), Some((0, false, 0)));
    }

    #[test]
    fn oversized_key_reply_is_reply_too_large() {
        let d = director();
        version(
            &d,
            OP_REG_SET_VALUE,
            &encode_reg_set_value(K, "big", 3, &vec![0u8; 5000]),
        );
        let (st, r) = dispatch_director(&d, OP_REG_KEY, &encode_reg_path(K), 0, 4096, None);
        assert_eq!(st, ST_REPLY_TOO_LARGE);
        assert!(r.is_empty());
        // It fits a larger ring.
        let (st, _) = dispatch_director(&d, OP_REG_KEY, &encode_reg_path(K), 0, 8192, None);
        assert_eq!(st, ST_OK);
        // The boundary: a reply of exactly payload_cap - 8 bytes fits.
        let len = ok(&d, OP_REG_KEY, &encode_reg_path(K)).len() as u32;
        let at = |cap| dispatch_director(&d, OP_REG_KEY, &encode_reg_path(K), 0, cap, None).0;
        assert_eq!(at(len + 8), ST_OK);
        assert_eq!(at(len + 7), ST_REPLY_TOO_LARGE);
    }

    /// Readers on several workers see each write whole. Phase one has a single writer whose
    /// write `n` stores `n` and gets version `n`, so a reply pairing a value with another
    /// version is a torn read. Phase two adds a second writer on another key: the writes
    /// are serialised, each bumping the version exactly once.
    #[test]
    fn concurrent_readers_during_writes_see_consistent_snapshots() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let d = Arc::new(director());
        let k2 = format!(r"{K}\Other");
        let set = |d: &Director, path: &str, n: u64| {
            version(
                d,
                OP_REG_SET_VALUE,
                &encode_reg_set_value(path, "n", 11, &n.to_le_bytes()),
            )
        };
        let readers = |d: &Arc<Director>, stop: &Arc<AtomicBool>, strict: bool| {
            (0..4)
                .map(|_| {
                    let d = d.clone();
                    let stop = stop.clone();
                    std::thread::spawn(move || {
                        let mut last = 0u64;
                        let mut reads = 0u64;
                        while !stop.load(Ordering::Relaxed) || reads == 0 {
                            let (node, v) =
                                decode_reg_key_reply(&ok(&d, OP_REG_KEY, &encode_reg_path(K)))
                                    .unwrap();
                            assert!(v >= last, "version went backwards");
                            last = v;
                            if let (true, Some(n)) = (strict, node) {
                                let got =
                                    u64::from_le_bytes(n.values[0].data[..].try_into().unwrap());
                                assert_eq!(got, v, "value and version from different states");
                            }
                            let r = ok(&d, OP_REG_LOOKUP, &encode_reg_path(K));
                            let (_, _, lv) = decode_reg_lookup_reply(&r).unwrap();
                            assert!(lv >= last);
                            let r = ok(&d, OP_REG_CHANGED, &encode_reg_changed(K, true, 0));
                            let (_, cv) = decode_reg_changed_reply(&r).unwrap();
                            assert!(cv >= lv);
                            last = cv;
                            reads += 1;
                        }
                        reads
                    })
                })
                .collect::<Vec<_>>()
        };

        // Phase one: one writer.
        let stop = Arc::new(AtomicBool::new(false));
        let rs = readers(&d, &stop, true);
        for n in 1..=300u64 {
            assert_eq!(set(&d, K, n), n);
        }
        stop.store(true, Ordering::Relaxed);
        for r in rs {
            assert!(r.join().unwrap() > 0);
        }

        // Phase two: two writers on different keys.
        let stop = Arc::new(AtomicBool::new(false));
        let rs = readers(&d, &stop, false);
        let ws: Vec<_> = [K.to_string(), k2.clone()]
            .into_iter()
            .map(|path| {
                let d = d.clone();
                std::thread::spawn(move || {
                    let mut prev = 0;
                    for n in 0..300u64 {
                        let v = set(&d, &path, n);
                        assert!(v > prev);
                        prev = v;
                    }
                })
            })
            .collect();
        for w in ws {
            w.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        for r in rs {
            assert!(r.join().unwrap() > 0);
        }
        let (_, _, v) =
            decode_reg_lookup_reply(&ok(&d, OP_REG_LOOKUP, &encode_reg_path(K))).unwrap();
        assert_eq!(v, 900, "every write bumped the version exactly once");
        for p in [K, k2.as_str()] {
            let (n, _) = decode_reg_key_reply(&ok(&d, OP_REG_KEY, &encode_reg_path(p))).unwrap();
            assert_eq!(n.unwrap().values[0].data, 299u64.to_le_bytes());
        }
    }
}
