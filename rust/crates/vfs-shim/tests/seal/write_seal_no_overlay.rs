//! The write seal against a provider graph with **no writable mount at all** (gate 4, Task 5).
//!
//! The name is historical: this was the write seal with no shim-local overlay
//! configured, the configuration in which a fall-through was not a misplacement
//! but a genuine escape — the shim-local engine answered `PassThrough` for a
//! write with no overlay, so the create was carried out by the real
//! `NtCreateFile` and a real file appeared **physically under the managed
//! root**. The shim-local overlay is gone (task C8), so there is one
//! configuration left, and the claim is unchanged: a write the director refuses
//! creates nothing on the real filesystem under the root.

use crate::fakedirector;

use vfs_shim::install;

#[test]
fn a_refused_write_creates_nothing_on_the_real_filesystem_under_the_root() {
    isolate!();
    let base = std::env::temp_dir().join(format!("vfs-write-seal-noov-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    // The physical directory exists and is writable by this process — so if
    // the create below reaches the real filesystem, it *succeeds*, which is
    // exactly the escape being ruled out. A non-existent directory would make
    // this test pass for the wrong reason.
    std::fs::create_dir_all(root.join("data")).unwrap();

    // A graph with no writable mount at all: every create under the root is
    // refused with `ST_NOT_FOUND`.
    fakedirector::install(&root, fakedirector::Fake::new(), 0);

    let hooks = install().expect("install");

    let escaped = root.join("data").join("escaped.bin");
    let result = std::fs::write(&escaped, b"content the provider graph never agreed to");

    // The real filesystem under the root is only observable with the detours
    // down — a hooked `exists()` asks the director, which answers "no" for
    // anything the VFS does not serve, and would pass vacuously.
    drop(hooks);

    // The substantive claim first: whatever status the call returned, no file
    // may have appeared.
    assert!(
        !escaped.exists(),
        "a write the director refused was carried out by the real filesystem instead: \
         {escaped:?} now physically exists under the managed root. That file is invisible \
         to every reader (the root seals what the provider graph does not serve), so it is \
         both an escape and a silent data loss"
    );
    let err = result.expect_err("a write under a managed root that no provider serves must fail");
    assert_eq!(
        err.raw_os_error(),
        Some(3), // ERROR_PATH_NOT_FOUND
        "expected ERROR_PATH_NOT_FOUND from the refused create, got {err:?}"
    );

    let _ = std::fs::remove_dir_all(&base);
}
