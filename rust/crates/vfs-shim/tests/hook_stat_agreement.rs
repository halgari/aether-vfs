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
    // Real on disk, hidden by the snapshot. Every API must refuse to see it.
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

    // ── a file that exists only in the VFS ──────────────────────────────────
    let virt = root.join("added.esm");
    let nt = format!(r"\??\{}", virt.display());

    assert!(
        std::fs::metadata(&virt).is_ok(),
        "std::fs::metadata could not see the virtual file"
    );
    assert_eq!(
        std::fs::metadata(&virt).unwrap().len(),
        PAYLOAD_LEN,
        "std::fs::metadata reported the wrong size"
    );

    let (st, _) = nt_query_attributes_abs(&nt);
    assert!(st >= 0, "NtQueryAttributesFile missed the virtual file: {st:#x}");

    let (st, size) = nt_query_full_attributes_abs(&nt);
    assert!(st >= 0, "NtQueryFullAttributesFile missed the virtual file: {st:#x}");
    assert_eq!(size as u64, PAYLOAD_LEN, "NtQueryFullAttributesFile size");

    for class in SIZED_CLASSES {
        let Some((st, size)) = nt_query_by_name_abs(&nt, class) else {
            continue; // export absent on this Windows build
        };
        assert!(st >= 0, "NtQueryInformationByName({class}) missed the virtual file: {st:#x}");
        assert_eq!(size as u64, PAYLOAD_LEN, "NtQueryInformationByName({class}) size");
    }

    // ── a real file the snapshot hides ──────────────────────────────────────
    let hidden = root.join("hidden.esp");
    let hidden_nt = format!(r"\??\{}", hidden.display());

    assert!(
        std::fs::metadata(&hidden).is_err(),
        "std::fs::metadata revealed a tombstoned file"
    );
    let (st, _) = nt_query_attributes_abs(&hidden_nt);
    assert!(st < 0, "NtQueryAttributesFile revealed a tombstoned file");
    let (st, _) = nt_query_full_attributes_abs(&hidden_nt);
    assert!(st < 0, "NtQueryFullAttributesFile revealed a tombstoned file");
    for class in SIZED_CLASSES {
        if let Some((st, _)) = nt_query_by_name_abs(&hidden_nt, class) {
            assert!(
                st < 0,
                "NtQueryInformationByName({class}) revealed a tombstoned file"
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
