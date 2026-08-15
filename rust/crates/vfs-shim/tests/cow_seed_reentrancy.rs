//! Copy-up's own file I/O must not be re-decided by the hook that asked for it
//! (gate 4, task 4).
//!
//! Single-test binary twice over: it installs the process-global detours
//! (`ENGINE` is a `OnceLock`, so one engine per process) *and* the
//! process-global `FuseClient`.
//!
//! **The shape being tested.** `Engine::cow_seed` runs inside `create_hook`,
//! and the file it writes is one the shim chose, not one the game named.
//! Nothing constrains that path to lie outside a managed root — an overlay
//! placed under the root is a perfectly ordinary session layout — so copy-up's
//! `File::create` is an NT open, made from inside a hook, on a path the hook
//! would classify as ours. Left unguarded it is answered by the VFS instead of
//! the filesystem: at best the bytes land somewhere other than where the very
//! same `decide_open` call is about to point the game, at worst it re-enters
//! copy-up and recurses (this project has lost a process to that twice
//! already — `vfs_redirect::OS_CONSULT_DEPTH` and `engine.rs`'s
//! `MAP_INIT_DEPTH`). `crate::hook::ShimIoGuard`, held across the whole seed,
//! is what sends those opens straight to the real ntdll.
//!
//! **Why the overlay is under the root here.** That is the configuration that
//! makes the guard observable. With the overlay outside the root (every other
//! test, and the usual deployment) an unguarded `File::create` is decided
//! `PassThrough` and works by accident, so the guard's presence changes
//! nothing you can assert on. Removing the guard makes this test fail; that is
//! the whole reason it is written this way.

mod fakedirector;

use fakedirector::{Fake, ReadStyle};
use std::io::Write;
use vfs_shim::{install, Engine};

const PROVIDER: &[u8] = b"bytes only the director has, copied up while a hook is on the stack";

#[test]
fn copy_up_writes_its_destination_without_re_entering_the_hooks() {
    let base = std::env::temp_dir().join(format!("vfs-cowseed-reent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    // Deliberately UNDER the managed root — see the module doc.
    let overlay = root.join("__overlay");
    std::fs::create_dir_all(root.join("Data")).unwrap();
    std::fs::create_dir_all(&overlay).unwrap();
    // Pre-create the destination's parent, so this test turns on copy-up's own
    // I/O and nothing else. `Overlay::ensure_parent`'s `create_dir_all` — which
    // `decide_open` calls just before copy-up, outside it, and whose result it
    // discards — is hooked here for the same reason and fails; that is a
    // separate weakness of the same family, noted in the task report, and
    // leaving it in the frame would make this test fail for a reason that is
    // not the one it is about.
    std::fs::create_dir_all(
        vfs_shim::overlay_layer_dir(&overlay, vfs_redirect::RootId::DEFAULT).join("data"),
    )
    .unwrap();

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "unrelated.txt".into(),
                kind: EntryKind::File,
                source: r"D:\nowhere\unrelated.txt".into(),
                size: 0,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    // A read-only provider: it serves the file but refuses OPEN_WRITE, which
    // is what routes the write through `try_fuse_create`'s fall-through into
    // the shim-local overlay — the only way a hooked process reaches copy-up.
    fakedirector::install(
        &root,
        Fake::new()
            .with("data/seedme.esp", PROVIDER.to_vec(), ReadStyle::Whole)
            .read_only(),
    );

    let engine =
        Engine::with_overlay(root.to_str().unwrap(), overlay.to_str().unwrap(), snapshot).unwrap();
    let hooks = install(engine).expect("install");

    // FILE_OPEN + write access: preserving, and the file must already exist —
    // so this open can only succeed if copy-up really did materialise it.
    let opened = std::fs::OpenOptions::new()
        .append(true)
        .open(root.join("Data").join("seedme.esp"));

    // Everything below reads paths under the managed root, so the detours have
    // to be gone before the assertions can see the real filesystem.
    drop(hooks);

    let mut f = opened.expect(
        "the preserving write should have been satisfied by a copy-up: copy-up read the \
         director, wrote the overlay file, and the redirected open then found it. A \
         not-found here means copy-up's own `File::create` did not reach the real \
         filesystem — it was re-decided by the hooks and its bytes went somewhere else.",
    );
    f.write_all(b"!").unwrap();
    drop(f);

    let dest = vfs_shim::overlay_layer_dir(&overlay, vfs_redirect::RootId::DEFAULT)
        .join("data")
        .join("seedme.esp");
    let mut want = PROVIDER.to_vec();
    want.push(b'!');
    assert!(
        std::fs::read(&dest).unwrap_or_default() == want,
        "the overlay copy at {dest:?} does not hold the director's bytes followed by the \
         game's append"
    );

    let _ = std::fs::remove_dir_all(&base);
}
