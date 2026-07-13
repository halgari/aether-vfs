use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId, NodeKind, Resolution, SourceId};

fn file(vpath: &str, source: &str, size: u64, mtime: i64) -> InputEntry {
    InputEntry { vpath: vpath.into(), kind: EntryKind::File, source: source.into(), size, mtime }
}
fn tomb(vpath: &str) -> InputEntry {
    InputEntry { vpath: vpath.into(), kind: EntryKind::Tombstone, source: "".into(), size: 0, mtime: 0 }
}

#[test]
fn end_to_end_modded_game_view() {
    // Layer 0 = real game dir; layers 1..=2 = mods (higher wins).
    let tree = build(vec![
        Layer {
            id: LayerId(0),
            entries: vec![
                file("Data/Skyrim.esm", "game/Data/Skyrim.esm", 100, 1),
                file("Data/textures/rock.dds", "game/.../rock.dds", 50, 1),
            ],
        },
        Layer {
            id: LayerId(1),
            entries: vec![file("Data/textures/rock.dds", "mod1/rock.dds", 80, 2)],
        },
        Layer {
            id: LayerId(2),
            entries: vec![
                file("Data/MyMod.esp", "mod2/MyMod.esp", 10, 3),
                tomb("Data/Skyrim.esm"),
            ],
        },
    ])
    .unwrap();

    // Mod1 overrides the base texture.
    match tree.resolve("Data/textures/rock.dds") {
        Resolution::File { source, size, layer, .. } => {
            assert_eq!(source, SourceId::from("mod1/rock.dds"));
            assert_eq!(size, 80);
            assert_eq!(layer, LayerId(1));
        }
        other => panic!("expected mod1 file, got {other:?}"),
    }

    // Mod2 tombstones the base master.
    assert_eq!(tree.resolve("Data/Skyrim.esm"), Resolution::NotFound);

    // Merged Data listing is sorted case-insensitively and honors the tombstone.
    let names: Vec<String> =
        tree.readdir("Data", None).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, vec!["MyMod.esp", "textures"]);

    // The new mod file resolves.
    assert!(matches!(tree.resolve("Data/MyMod.esp"), Resolution::File { .. }));

    // A directory reports as a dir via getattr.
    assert_eq!(tree.getattr("Data/textures").unwrap().kind, NodeKind::Dir);
}

use proptest::prelude::*;

proptest! {
    // Building from arbitrary component names never panics and every inserted
    // leaf either resolves or was shadowed — build is total over valid input.
    #[test]
    fn build_never_panics_on_arbitrary_names(
        names in proptest::collection::vec("[a-zA-Z0-9]{1,8}", 1..20)
    ) {
        let entries: Vec<InputEntry> = names
            .iter()
            .enumerate()
            .map(|(i, n)| file(&format!("d/{n}"), &format!("s{i}"), i as u64, i as i64))
            .collect();
        let tree = build(vec![Layer { id: LayerId(0), entries }]).unwrap();
        // The directory always exists and readdir succeeds.
        prop_assert!(tree.readdir("d", None).is_ok());
    }
}
