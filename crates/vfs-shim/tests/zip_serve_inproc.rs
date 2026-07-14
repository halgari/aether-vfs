//! In-process proof: a zip-window snapshot makes `std::fs::read` of a virtual
//! path return the exact bytes from a window inside a real container file.
use vfs_shim::install;

#[test]
fn reads_a_zip_window_through_the_hook() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-zipin-{pid}"));
    let root = base.join("gameroot");
    std::fs::create_dir_all(&root).unwrap();

    // A "container": 5 filler bytes then the payload we want to serve.
    let container = base.join("container.bin");
    let payload = b"BYTES-STRAIGHT-FROM-THE-CONTAINER";
    let mut blob = vec![b'.'; 5];
    blob.extend_from_slice(payload);
    std::fs::write(&container, &blob).unwrap();

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId, SourceId};
        let src = SourceId::new(vfs_core::encode_zip_window(
            5,
            &container.to_string_lossy(),
        ));
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "asset.dat".into(),
                kind: EntryKind::File,
                source: src,
                size: payload.len() as u64,
                mtime: 1,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    let engine = vfs_shim::Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install hooks");

    let virtual_path = root.join("asset.dat");
    let got = std::fs::read(&virtual_path).expect("read virtual zip-backed file");
    assert_eq!(got, payload, "served bytes must equal the container window");
}
