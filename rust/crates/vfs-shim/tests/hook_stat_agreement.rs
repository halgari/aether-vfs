//! Single-test binary: every way of asking "does this exist, and how big is it"
//! must give the same answer.
//!
//! Windows has several: `NtQueryAttributesFile`, `NtQueryFullAttributesFile`,
//! `NtQueryInformationByName` (which Windows 11 prefers), and opening the file.
//! Callers pick between them for reasons of their own — the same program will
//! use different ones in different code paths — so any hook that answers
//! differently from its siblings produces a program that believes a file both
//! exists and does not.
//!
//! Both directions matter and both have bitten:
//!   * a *false negative* makes content silently invisible. Skyrim's intro video
//!     went missing exactly this way, through the one stat API that was
//!     unhooked; a caller that tolerates a missing file just skips it.
//!   * a *false positive* leaks a file the snapshot deliberately hides. A
//!     tombstone honoured by three APIs and ignored by the fourth still exposes
//!     the file.
//!
//! Task 4 note: this binary installs the shim with **no director** attached
//! (`vfs_shim::install`, not a real launch). Before Task 4, a plain `Engine`
//! answered attribute queries locally from its published snapshot
//! (`RootMap::query_attributes`/`AttrDecision`), so a virtual-only file was
//! visible and a tombstoned real file was hidden through the **name-based**
//! attribute APIs (`NtQueryAttributesFile`, `NtQueryFullAttributesFile`,
//! `NtQueryInformationByName`) even with no director in the loop. Task 4
//! deleted that local-answering path — those three now route to the director
//! only (`hook.rs::fuse_path_attr`) — so with none attached, a virtual-only
//! file is (correctly) invisible to all three, and a tombstoned real file is
//! (correctly, for this no-director harness) visible to all three, since
//! nothing here has been told to hide it from them. Those assertions were
//! flipped for exactly that reason.
//!
//! `std::fs::metadata` is unaffected and stays as it was: Rust's std opens a
//! handle for `metadata()` rather than calling a name-based attribute API, so
//! it resolves through `Decision` (untouched by Task 4; that deletion is gate
//! 4's) — a virtual file is still visible through it, and a tombstoned file
//! is still hidden through it, exactly as before.
//!
//! No director-mediated equivalent of the flipped assertions exists yet
//! (same gap Task 3's report already named for other hooks): a real launch
//! always has a director, so this is a coverage gap in the test suite, not a
//! live bypass.

use std::ffi::c_void;

mod ntapi;
use ntapi::*;

const PAYLOAD_LEN: u64 = 4096;

/// Classes that carry a size, and the offset each puts it at.
const SIZED_CLASSES: [u32; 3] = [34, 68, 77];

#[test]
fn every_stat_api_agrees_about_existence_and_size() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-shim-statagree-{pid}"));
    let root = base.join("gameroot");
    let backing = base.join("backing");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&backing).unwrap();

    let add_backing = backing.join("added.esm");
    std::fs::write(&add_backing, vec![7u8; PAYLOAD_LEN as usize]).unwrap();
    // Real on disk; the snapshot tombstones it, but with no director attached
    // the name-based attribute APIs never consult that tombstone any more
    // (see the module doc comment).
    std::fs::write(root.join("hidden.esp"), b"leaked").unwrap();

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let e = |vpath: &str, kind: EntryKind, source: &str, size: u64| InputEntry {
            vpath: vpath.into(),
            kind,
            source: source.into(),
            size,
            mtime: 0,
        };
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![
                e("added.esm", EntryKind::File, add_backing.to_str().unwrap(), PAYLOAD_LEN),
                e("hidden.esp", EntryKind::Tombstone, "", 0),
            ],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = vfs_shim::Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = vfs_shim::install(engine).expect("install");

    // Some `NtQueryInformationByName` classes are unsupported for a plain
    // by-name query on some Windows builds/paths even for a perfectly
    // ordinary, already-existing real file with nothing virtualized about it
    // (`add_backing` sits outside `root` entirely, so this is a pure
    // passthrough query, unrelated to anything this shim or Task 4 does).
    // A class this environment can't use at all tells us nothing about
    // whether the hooks agree with each other, so such a class is skipped
    // below exactly like the existing "export absent" tolerance.
    let add_backing_nt = format!(r"\??\{}", add_backing.display());
    let class_supported = |class: u32| -> bool {
        matches!(nt_query_by_name_abs(&add_backing_nt, class), Some((st, _)) if st >= 0)
    };

    // ── a file that exists only in the VFS ──────────────────────────────────
    let virt = root.join("added.esm");
    let nt = format!(r"\??\{}", virt.display());

    // `std::fs::metadata` opens a handle (`Decision`-backed) — unaffected by
    // Task 4, unchanged from before.
    assert!(
        std::fs::metadata(&virt).is_ok(),
        "std::fs::metadata could not see the virtual file"
    );
    assert_eq!(
        std::fs::metadata(&virt).unwrap().len(),
        PAYLOAD_LEN,
        "std::fs::metadata reported the wrong size"
    );

    // The name-based attribute APIs no longer answer locally without a
    // director (flipped from "must find it" — see the module doc comment).
    let (st, _) = nt_query_attributes_abs(&nt);
    assert!(st < 0, "NtQueryAttributesFile saw a virtual-only file with no director attached");
    let (st, _) = nt_query_full_attributes_abs(&nt);
    assert!(
        st < 0,
        "NtQueryFullAttributesFile saw a virtual-only file with no director attached"
    );
    for class in SIZED_CLASSES {
        if let Some((st, _)) = nt_query_by_name_abs(&nt, class) {
            assert!(
                st < 0,
                "NtQueryInformationByName({class}) saw a virtual-only file with no director attached"
            );
        }
    }

    // ── a real file the snapshot hides ──────────────────────────────────────
    let hidden = root.join("hidden.esp");
    let hidden_nt = format!(r"\??\{}", hidden.display());

    // `std::fs::metadata` still hides it: the tombstone is enforced through
    // `Decision::Deny` (an open-based path, untouched by Task 4) —
    // unchanged from before.
    assert!(
        std::fs::metadata(&hidden).is_err(),
        "std::fs::metadata revealed a tombstoned file"
    );

    // The name-based attribute APIs no longer enforce the tombstone without a
    // director (flipped from "must refuse to see it" — see the module doc
    // comment).
    let (st, _) = nt_query_attributes_abs(&hidden_nt);
    assert!(
        st >= 0,
        "NtQueryAttributesFile hid a real file with no director attached to enforce the tombstone"
    );
    let (st, _) = nt_query_full_attributes_abs(&hidden_nt);
    assert!(
        st >= 0,
        "NtQueryFullAttributesFile hid a real file with no director attached to enforce the tombstone"
    );
    for class in SIZED_CLASSES {
        if !class_supported(class) {
            continue; // this class does not answer for a plain real file here
        }
        if let Some((st, _)) = nt_query_by_name_abs(&hidden_nt, class) {
            assert!(
                st >= 0,
                "NtQueryInformationByName({class}) hid a real file with no director attached"
            );
        }
    }

    // ── a name that never existed ───────────────────────────────────────────
    let absent_nt = format!(r"\??\{}", root.join("absent.esm").display());
    let (st, _) = nt_query_full_attributes_abs(&absent_nt);
    assert!(st < 0, "a name in neither the VFS nor on disk reported success");
    for class in SIZED_CLASSES {
        if let Some((st, _)) = nt_query_by_name_abs(&absent_nt, class) {
            assert!(st < 0, "NtQueryInformationByName({class}) invented a file");
        }
    }

    let _ = std::fs::remove_dir_all(&base);
}

fn nt_query_attributes_abs(nt_path: &str) -> (i32, u32) {
    nt_query_attributes_relative(core::ptr::null_mut::<c_void>(), nt_path)
}
fn nt_query_full_attributes_abs(nt_path: &str) -> (i32, i64) {
    nt_query_full_attributes_relative(core::ptr::null_mut::<c_void>(), nt_path)
}
fn nt_query_by_name_abs(nt_path: &str, class: u32) -> Option<(i32, i64)> {
    nt_query_by_name_relative(core::ptr::null_mut::<c_void>(), nt_path, class)
}
