//! Single-test binary: writes go to the overlay (create + copy-on-write); the
//! mod backing file is never mutated.
use std::io::Write;
use vfs_shim::{install, Engine};

#[test]
fn writes_land_in_overlay_with_cow() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-shim-write-{pid}"));
    let root = base.join("root");
    let overlay = base.join("overlay");
    let mods = base.join("mods");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&overlay).unwrap();
    std::fs::create_dir_all(&mods).unwrap();

    // A mod (virtual) file backed on disk, mapped by the snapshot.
    let backing = mods.join("mod_backing.esp");
    std::fs::write(&backing, b"ORIG").unwrap();

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "mod.esp".into(),
                kind: EntryKind::File,
                source: backing.to_str().unwrap().into(),
                size: 4,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine =
        Engine::with_overlay(root.to_str().unwrap(), overlay.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install");

    // --- Create a brand-new file under the root ---
    let newfile = root.join("created.txt");
    {
        let mut f = std::fs::File::create(&newfile).expect("create new");
        f.write_all(b"NEW").unwrap();
    }
    // Readable through the virtual path...
    assert_eq!(std::fs::read(&newfile).unwrap(), b"NEW", "new file readable via VFS");
    // ...and physically it lives in the overlay, not the real root (the overlay
    // path is outside the managed root, so this read is un-virtualized).
    assert_eq!(std::fs::read(overlay.join("created.txt")).unwrap(), b"NEW", "landed in overlay");

    // --- Copy-on-write modify of a mod file ---
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(root.join("mod.esp")).expect("open mod for append");
        f.write_all(b"X").unwrap();
    }
    // Virtual read reflects the modification...
    assert_eq!(std::fs::read(root.join("mod.esp")).unwrap(), b"ORIGX", "COW modification visible");
    // ...the overlay holds the materialized+modified copy...
    assert_eq!(std::fs::read(overlay.join("mod.esp")).unwrap(), b"ORIGX", "overlay has COW copy");
    // ...and the shared mod backing is untouched.
    assert_eq!(std::fs::read(&backing).unwrap(), b"ORIG", "backing must not be mutated");
}
