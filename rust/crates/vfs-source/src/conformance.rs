//! Behavioral suite every source (in-proc or remote) must pass.

use std::sync::Arc;

use vfs_protocol::{Backend, KIND_DIR, KIND_FILE, OPEN_READ, ST_NOT_FOUND};

/// Run the full suite against `backend`. Panics on failure (for `#[test]`).
pub fn assert_conformance(backend: Arc<dyn Backend>) {
    // missing path
    assert_eq!(backend.getattr("no-such-file").unwrap(), None);

    // file getattr
    let st = backend
        .getattr("hello.txt")
        .unwrap()
        .expect("hello.txt present");
    assert_eq!(st.kind, KIND_FILE);
    assert_eq!(st.size, 5);

    // dir getattr
    let root = backend.getattr("").unwrap().expect("root dir");
    assert_eq!(root.kind, KIND_DIR);

    // readdir root contains hello.txt
    let entries = backend.readdir("").unwrap();
    assert!(
        entries.iter().any(|e| e.name.eq_ignore_ascii_case("hello.txt")),
        "readdir missing hello.txt: {entries:?}"
    );

    // open + full read
    let (h, size, is_dir) = backend.open("hello.txt", OPEN_READ).unwrap();
    assert!(!is_dir && size == 5);
    let mut buf = [0u8; 16];
    let n = backend.read(h, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"hello");

    // offset read
    let n = backend.read(h, 2, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"llo");

    // EOF
    let n = backend.read(h, 5, &mut buf).unwrap();
    assert_eq!(n, 0);

    backend.release(h).unwrap();

    // open missing
    let err = backend.open("missing.bin", OPEN_READ).unwrap_err();
    assert_eq!(err, ST_NOT_FOUND);

    // readdir on file
    let _ = backend.readdir("hello.txt"); // may be not_a_dir
}

/// Populate a temp dir with the fixture layout the suite expects.
pub fn write_fixture_tree(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("hello.txt"), b"hello").unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub").join("a.bin"), b"abc").unwrap();
}
