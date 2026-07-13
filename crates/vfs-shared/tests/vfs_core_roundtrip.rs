#![cfg(feature = "bridge")]

use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId, Resolution};
use vfs_shared::bridge::flatten;
use vfs_shared::{NodeKind, SnapResolution, SnapshotReader};

fn file(vpath: &str, source: &str, size: u64, mtime: i64) -> InputEntry {
    InputEntry { vpath: vpath.into(), kind: EntryKind::File, source: source.into(), size, mtime }
}
fn tomb(vpath: &str) -> InputEntry {
    InputEntry { vpath: vpath.into(), kind: EntryKind::Tombstone, source: "".into(), size: 0, mtime: 0 }
}

#[test]
fn snapshot_answers_match_vfs_core() {
    let tree = build(vec![
        Layer {
            id: LayerId(0),
            entries: vec![
                file("Data/Skyrim.esm", "game/Skyrim.esm", 100, 1),
                file("Data/textures/rock.dds", "game/rock.dds", 50, 1),
            ],
        },
        Layer {
            id: LayerId(1),
            entries: vec![file("Data/textures/rock.dds", "mod1/rock.dds", 80, 2)],
        },
        Layer {
            id: LayerId(2),
            entries: vec![file("Data/MyMod.esp", "mod2/MyMod.esp", 10, 3), tomb("Data/Skyrim.esm")],
        },
    ])
    .unwrap();

    let img = flatten(&tree);
    let r = SnapshotReader::open(&img).unwrap();

    // The overridden texture resolves to mod1 in both.
    match (tree.resolve("Data/textures/rock.dds"), r.resolve(&["data", "textures", "rock.dds"])) {
        (
            Resolution::File { source: cs, size: csz, layer: cl, .. },
            SnapResolution::File { source: ss, size: ssz, layer: sl, .. },
        ) => {
            assert_eq!(cs, vfs_core::SourceId::from("mod1/rock.dds"));
            assert_eq!(ss, b"mod1/rock.dds");
            assert_eq!(csz, ssz);
            assert_eq!(cl.0, sl);
        }
        other => panic!("resolution mismatch: {other:?}"),
    }

    // The tombstoned master is gone in both. (Reader keys are folded: "skyrim.esm".)
    assert_eq!(tree.resolve("Data/Skyrim.esm"), Resolution::NotFound);
    assert_eq!(r.resolve(&["data", "skyrim.esm"]), SnapResolution::NotFound);

    // Merged Data listing matches (case-insensitive order, tombstone honored).
    let core_names: Vec<String> =
        tree.readdir("Data", None).unwrap().into_iter().map(|e| e.name).collect();
    let snap_names: Vec<String> =
        r.readdir(&["data"]).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(core_names, snap_names);
    assert_eq!(snap_names, vec!["MyMod.esp", "textures"]);

    // getattr kinds agree for a directory.
    assert_eq!(r.getattr(&["data", "textures"]).unwrap().kind, NodeKind::Dir);
}
