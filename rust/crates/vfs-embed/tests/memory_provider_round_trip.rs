//! The design spec's `memory({...})` demonstration, built through `vfs_embed`
//! alone: a host hands in bytes, writes through the provider the way the
//! director's own write path does, and reads back what was written through
//! the host-facing accessor — the whole point of a `memory` provider being a
//! first-class participant rather than a test fixture.
//!
//! ```python
//! inis = vfs.memory({"Skyrim.ini": ini_bytes})
//! ...
//! print(inis.read("Skyrim.ini"))     # what the game actually wrote
//! ```
//!
//! `docs/superpowers/specs/2026-08-13-pluggable-providers-design.md` §8.

use std::sync::Arc;

use vfs_embed::{MemoryProvider, RootId, Session, OPEN_WRITE};

#[test]
fn a_memory_provider_round_trips_a_write_back_to_the_host() {
    let session_base =
        std::env::temp_dir().join(format!("vfs-embed-memrt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&session_base);

    let mut session = Session::new();
    session.set_root(session_base.join("root"));
    session.set_overlay(session_base.join("overlay"));
    session.set_state_dir(session_base.join("state"));

    // The host hands in bytes — no disk, no zip, nothing else backing this
    // mount.
    let inis = Arc::new(MemoryProvider::from_files([(
        "Skyrim.ini",
        b"ORIGINAL-INI".as_slice(),
    )]));
    session.mount("", inis).expect("mount memory provider");

    // The write, exactly as the director performs an in-place edit: open for
    // write through the kernel against the composed root. `MemoryProvider`
    // declares `Access::ReadWrite`, so this needs no overlay write layer —
    // the mount itself takes the write directly (`Director::open`'s
    // `capabilities().access < Access::ReadWrite` gate passes).
    let (fh, size, is_dir) = session
        .kernel()
        .open(RootId::DEFAULT, "Skyrim.ini", OPEN_WRITE)
        .expect("an Access::ReadWrite mount must accept an in-place write directly");
    assert!(!is_dir);
    assert_eq!(size, 12, "the handle opens onto the bytes the host supplied");
    session.kernel().write(fh, 0, b"EDITED-INI!!").expect("write");
    session.kernel().close(fh).expect("close");

    // The host-facing read-back — not a peek at the provider's internals,
    // but the same accessor `embed_api.rs`'s two-root test uses
    // (`Session::read_file`), which is what a binding would expose as
    // `inis.read(...)`.
    assert_eq!(
        session.read_file("Skyrim.ini").unwrap(),
        b"EDITED-INI!!",
        "the host must read back exactly what was written through the provider, not the \
         original construction bytes"
    );

    let _ = std::fs::remove_dir_all(&session_base);
}
