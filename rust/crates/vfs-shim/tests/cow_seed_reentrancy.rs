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
//!
//! **Why `decide_open` is called directly rather than through an open**
//! (gate 4, Task 5). This test used to reach copy-up the way a game did:
//! a write open the director refused fell through to `Engine::decide_open`.
//! That fall-through is now sealed — a director-refused write is a hard NT
//! failure and never reaches the engine — so with a director attached the only
//! remaining hook route into copy-up is a DRM/identity exception
//! (`steam_appid.txt` and friends, which return `None` from `try_fuse_create`
//! before the ring is consulted; `cow_seed_reporting` uses exactly that). It
//! cannot be used *here*, because this test's whole point is an overlay
//! **under** the managed root: the overlay copy of a DRM-named file is itself
//! DRM-named, so every probe of it takes the exception too, is never sealed by
//! the director, and resolves to a one-level-deeper overlay path — unbounded
//! recursion, reproduced as a stack overflow while writing this. (Not a defect
//! introduced here, and not live today — `skyrim-live` puts its overlay beside
//! the root, not inside it — but worth knowing before gate 5 touches those
//! exceptions.)
//!
//! So the *entry* is a direct call and everything that matters stays intact:
//! the detours are installed, so copy-up's `File::create` is still an NT open
//! made with the hooks live, which is the claim. The engine that performs the
//! copy-up is a second instance built from the same roots and the same overlay
//! directory as the installed one, since `install` moves its engine into a
//! `OnceLock` the crate does not hand back.

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

    // The director is the only place these bytes exist, which is what makes
    // "the overlay copy holds them" mean copy-up really crossed the ring.
    fakedirector::install(
        &root,
        Fake::new().with("data/seedme.esp", PROVIDER.to_vec(), ReadStyle::Whole),
        // Inline: this fixture is a few dozen bytes, well under
        // `BULK_THRESHOLD`, so an arena would go unused. The transports are
        // covered in `cow_seed_reads_through_director`; what is under test
        // here is which *filesystem* copy-up's destination write reaches.
        0,
    );

    let build_engine = || {
        Engine::with_overlay(root.to_str().unwrap(), overlay.to_str().unwrap(), snapshot.clone())
            .unwrap()
    };
    // The engine the hooks consult…
    let hooks = install(build_engine()).expect("install");
    // …and an identically-configured one to drive copy-up from, since
    // `install` keeps the first (see the module doc).
    let engine = build_engine();

    // GENERIC_WRITE + FILE_OPEN: a preserving write to a file that must
    // already exist — the shape that asks for a copy-up. `decide_open` runs
    // copy-up inline, so by the time this returns the destination write has
    // already happened, with the detours installed.
    const GENERIC_WRITE: u32 = 0x4000_0000;
    let nt = format!(r"\??\{}", root.join("Data").join("seedme.esp").display());
    let decision = engine.decide_open(&nt, GENERIC_WRITE, vfs_redirect::FILE_OPEN);
    assert!(
        matches!(decision, vfs_redirect::Decision::Redirect { .. }),
        "the write must be redirected into the overlay for this test to mean anything, got \
         {decision:?}"
    );

    // Everything below reads paths under the managed root, so the detours have
    // to be gone before the assertions can see the real filesystem.
    drop(hooks);

    let dest = vfs_shim::overlay_layer_dir(&overlay, vfs_redirect::RootId::DEFAULT)
        .join("data")
        .join("seedme.esp");
    // The append the redirected open would have performed, done here directly:
    // it proves the materialised copy is a real, usable file at the redirect
    // target, not merely bytes somewhere.
    let mut f = std::fs::OpenOptions::new().append(true).open(&dest).expect(
        "copy-up should have materialised the overlay copy at the path the redirect points \
         at. A not-found here means copy-up's own `File::create` did not reach the real \
         filesystem — it was re-decided by the hooks and its bytes went somewhere else.",
    );
    f.write_all(b"!").unwrap();
    drop(f);

    let mut want = PROVIDER.to_vec();
    want.push(b'!');
    assert!(
        std::fs::read(&dest).unwrap_or_default() == want,
        "the overlay copy at {dest:?} does not hold the director's bytes followed by the \
         game's append"
    );

    let _ = std::fs::remove_dir_all(&base);
}
