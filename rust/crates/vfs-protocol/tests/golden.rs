//! Wire freeze: fixed inputs must encode to exactly the bytes in
//! `golden/vectors.txt` (one `name hex` line per vector).
//!
//! The file is the contract with the injected shim. If an encoder changes on
//! purpose, bump the ring `VERSION` where the layout changed, then regenerate
//! with `GOLDEN_UPDATE=1 cargo test -p vfs-protocol --test golden` and review
//! the diff (the update run itself fails, by design, so it cannot pass in CI). A changed existing line is a wire break.

use std::fmt::Write as _;
use vfs_protocol as P;
use vfs_protocol::shimcfg::{decode_config, encode_config, encode_config_full, StaticImport};
use vfs_protocol::{AttrResp, DirEntryWire, OpenResp, ReadReq, SetattrReq, WriteReq};
use vfs_registry::{Child, Node, Value};

fn sample_node() -> Node {
    let mut n = Node {
        values: vec![
            Value {
                name: "Path".into(),
                ty: 1,
                data: vec![1, 2, 3],
            },
            Value {
                name: String::new(),
                ty: 4,
                data: vec![],
            },
        ],
        value_tombstones: vec!["gone".into()],
        created: true,
        volatile: true,
        last_write: 0x0123_4567_89ab_cdef,
        ..Node::default()
    };
    n.children
        .insert("sub".into(), ("Sub".into(), Child::Present));
    n.children
        .insert("dead".into(), ("Dead".into(), Child::Tombstone));
    n
}

fn vectors() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (
            "shim-config-v2-root-runtime",
            encode_config(r"C:\GameLayers\runtime"),
        ),
        (
            "open-req-read-skyrim",
            P::encode_open_req(0, P::OPEN_READ, "Data/Skyrim.esm"),
        ),
        (
            "getattr-resp-file-123",
            P::encode_getattr_resp(&AttrResp {
                found: true,
                is_dir: false,
                size: 123,
                mtime: -7,
            }),
        ),
        (
            "read-req-fh7-off10-len4",
            P::encode_read_req(&ReadReq {
                fh: 7,
                offset: 10,
                len: 4,
            }),
        ),
        ("read-resp-abcd", P::encode_read_resp(b"abcd")),
        (
            "readdir-resp-two",
            P::encode_readdir_resp(&[
                DirEntryWire {
                    name: "a.esp".into(),
                    is_dir: false,
                    size: 10,
                    mtime: 1,
                },
                DirEntryWire {
                    name: "sub".into(),
                    is_dir: true,
                    size: 0,
                    mtime: 0,
                },
            ]),
        ),
        ("close-req-99", P::encode_close_req(99)),
        (
            "open-resp-fh42-size1000",
            P::encode_open_resp(&OpenResp {
                fh: 42,
                size: 1000,
                ..Default::default()
            }),
        ),
        (
            "read-resp-bulk-len5-off65536",
            P::encode_read_resp_bulk(5, 65536),
        ),
        (
            "write-req-fh7-off10-abc",
            P::encode_write_req(
                &WriteReq {
                    fh: 7,
                    offset: 10,
                    len: 3,
                },
                b"abc",
            ),
        ),
        ("write-resp-3", P::encode_write_resp(3)),
        (
            "mkdir-req-mode493-dir",
            P::encode_mkdir_req(0, 493, "sub/dir"),
        ),
        (
            "rename-req-a-b",
            P::encode_rename_req(0, "old.txt", "new.txt"),
        ),
        (
            "setattr-req-fh5-size100",
            P::encode_setattr_req(&SetattrReq { fh: 5, size: 100 }),
        ),
        ("ring-header-slots4-cap256", {
            use vfs_ipc::seg::OwnedSeg;
            let owned = OwnedSeg::new(4096);
            vfs_ipc::ring::init(owned.seg(), 4, 256).unwrap();
            owned
                .seg()
                .read_bytes(0, vfs_ipc::layout::RING_HEADER_SIZE)
                .unwrap()
        }),
        // Added with the descriptor's removal: the flags, stored names,
        // shim-config extras and every registry codec.
        (
            "open-resp-immutable-gen-0xa1b2c3d4",
            P::encode_open_resp(&OpenResp {
                fh: 42,
                size: 1000,
                is_dir: false,
                immutable: true,
                mount_gen: 0xA1B2_C3D4,
            }),
        ),
        ("path-req-root1-a", P::encode_path_req(1, "a")),
        (
            "names-req-root2-skip1",
            P::encode_names_req(2, 1, "Data/Textures"),
        ),
        (
            "names-resp-two",
            P::encode_names_resp(&["Data".to_string(), "Skyrim.esm".to_string()]),
        ),
        (
            "shim-config-v2-one-static-import",
            encode_config_full(
                r"C:\GameLayers\runtime",
                &[StaticImport {
                    dll_name: "d3d11.dll".into(),
                    backing_path: r"C:\GameLayers\d3d11.dll".into(),
                }],
            ),
        ),
        (
            "reg-path",
            P::encode_reg_path(r"\Registry\Machine\Software"),
        ),
        (
            "reg-set-value",
            P::encode_reg_set_value(r"\Registry\Machine\Software", "Name", 3, &[9, 8, 7]),
        ),
        (
            "reg-delete-value",
            P::encode_reg_delete_value(r"\Registry\Machine\Software", "Name"),
        ),
        (
            "reg-create-key-volatile",
            P::encode_reg_create_key(r"\Registry\Machine\Software", true),
        ),
        (
            "reg-rename-key",
            P::encode_reg_rename_key(r"\Registry\Machine\Software", "Other"),
        ),
        (
            "reg-changed-subtree-since77",
            P::encode_reg_changed(r"\Registry\Machine", true, 77),
        ),
        (
            "reg-changed-reply-true-5",
            P::encode_reg_changed_reply(true, 5),
        ),
        ("reg-version-reply-9", P::encode_reg_version_reply(9)),
        (
            "reg-lookup-reply-state1-below-42",
            P::encode_reg_lookup_reply(1, true, 42),
        ),
        ("reg-key-reply-none-13", P::encode_reg_key_reply(None, 13)),
        (
            "reg-key-reply-sample-node-12",
            P::encode_reg_key_reply(Some(&sample_node()), 12),
        ),
    ]
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn render() -> String {
    let mut s = String::new();
    for (name, bytes) in vectors() {
        let _ = writeln!(s, "{name} {}", hex(&bytes));
    }
    s
}

#[test]
fn encoders_match_committed_golden() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/vectors.txt");
    if std::env::var_os("GOLDEN_UPDATE").is_some() {
        std::fs::write(path, render()).unwrap();
        panic!("rewrote tests/golden/vectors.txt; review the diff and rerun without GOLDEN_UPDATE");
    }
    let committed = std::fs::read_to_string(path).unwrap().replace("\r\n", "\n");
    let rendered = render();
    for (got, want) in rendered.lines().zip(committed.lines()) {
        assert_eq!(got, want, "golden vector drifted: this is a wire break");
    }
    assert_eq!(
        rendered.lines().count(),
        committed.lines().count(),
        "vector count differs from tests/golden/vectors.txt"
    );
}

/// Decoders accept what the golden bytes say, not just what today's encoder
/// emits: a pinned vector decodes back to its inputs.
#[test]
fn golden_bytes_decode_to_their_inputs() {
    let committed = include_str!("golden/vectors.txt");
    let line = |name: &str| -> Vec<u8> {
        let l = committed
            .lines()
            .find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix(' ')))
            .unwrap();
        (0..l.len() / 2)
            .map(|i| u8::from_str_radix(&l[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    };
    assert_eq!(
        P::decode_open_req(&line("open-req-read-skyrim")),
        Some((0, P::OPEN_READ, "Data/Skyrim.esm".to_string()))
    );
    assert_eq!(
        P::decode_getattr_resp(&line("getattr-resp-file-123")),
        Some(AttrResp {
            found: true,
            is_dir: false,
            size: 123,
            mtime: -7
        })
    );
    assert_eq!(
        P::decode_read_req(&line("read-req-fh7-off10-len4")),
        Some(ReadReq {
            fh: 7,
            offset: 10,
            len: 4
        })
    );
    assert_eq!(
        P::decode_read_resp(&line("read-resp-abcd")),
        Some(b"abcd".to_vec())
    );
    assert_eq!(
        P::decode_readdir_resp(&line("readdir-resp-two")).map(|v| v.len()),
        Some(2)
    );
    assert_eq!(P::decode_close_req(&line("close-req-99")), Some(99));
    assert_eq!(P::decode_write_resp(&line("write-resp-3")), Some(3));
    assert_eq!(
        P::decode_open_resp(&line("open-resp-immutable-gen-0xa1b2c3d4")),
        Some(OpenResp {
            fh: 42,
            size: 1000,
            is_dir: false,
            immutable: true,
            mount_gen: 0xA1B2_C3D4
        })
    );
    let cfg = decode_config(&line("shim-config-v2-one-static-import")).unwrap();
    assert_eq!(cfg.root, r"C:\GameLayers\runtime");
    assert_eq!(
        cfg.static_imports,
        vec![StaticImport {
            dll_name: "d3d11.dll".into(),
            backing_path: r"C:\GameLayers\d3d11.dll".into(),
        }]
    );
    assert_eq!(
        P::decode_names_req(&line("names-req-root2-skip1")),
        Some((2, 1, "Data/Textures".to_string()))
    );
    assert_eq!(
        P::decode_reg_set_value(&line("reg-set-value")),
        Some((r"\Registry\Machine\Software", "Name", 3, &[9u8, 8, 7][..]))
    );
    assert_eq!(
        P::decode_reg_key_reply(&line("reg-key-reply-sample-node-12")),
        Some((Some(sample_node()), 12))
    );
    assert_eq!(
        P::decode_reg_lookup_reply(&line("reg-lookup-reply-state1-below-42")),
        Some((1, true, 42))
    );
    assert_eq!(
        P::decode_reg_changed(&line("reg-changed-subtree-since77")),
        Some((r"\Registry\Machine", true, 77))
    );
}
