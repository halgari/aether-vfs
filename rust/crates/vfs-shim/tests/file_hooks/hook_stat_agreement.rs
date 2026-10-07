//! Runs in its own process: every way of asking "does this exist, and how big is it"
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
//! Under a managed root all of them are answered by the director: the name-based queries by its
//! `getattr` (`hook/file_attr.rs::stat_by_path`), an open by its `open`. Before task C8 this
//! binary ran with no director and the name-based assertions had been flipped to "nothing
//! answers" while `std::fs::metadata` still went through the snapshot, so the four APIs were
//! asserted to *disagree*. Through the fake director they agree again, in both directions: a
//! served file is visible with its size to every API, and a real file under the root that the
//! director does not serve (where a snapshot tombstone used to be) is invisible to every API.

use std::ffi::c_void;

use crate::fakedirector;
use crate::ntapi;
use fakedirector::{Fake, ReadStyle};
use ntapi::*;

const PAYLOAD_LEN: u64 = 4096;

/// Classes that carry a size, and the offset each puts it at.
const SIZED_CLASSES: [u32; 3] = [34, 68, 77];

#[test]
fn every_stat_api_agrees_about_existence_and_size() {
    isolate!();
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-shim-statagree-{pid}"));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("gameroot");
    std::fs::create_dir_all(&root).unwrap();
    // Outside every root: a plain real file, used only to probe which by-name classes this
    // host answers at all.
    let add_backing = base.join("probe.esm");
    std::fs::write(&add_backing, b"probe").unwrap();
    // Real on disk under the root, and not served: every API must call it absent.
    std::fs::write(root.join("hidden.esp"), b"leaked").unwrap();

    fakedirector::install(
        &root,
        Fake::new().with(
            "added.esm",
            vec![7u8; PAYLOAD_LEN as usize],
            ReadStyle::Whole,
        ),
        0,
    );
    let _guard = vfs_shim::install().expect("install");

    // Some `NtQueryInformationByName` classes are unsupported for a plain
    // by-name query on some Windows builds/paths even for a perfectly
    // ordinary, already-existing real file with nothing virtualized about it
    // (`add_backing` sits outside `root` entirely, so this is a pure
    // passthrough query, unrelated to anything this shim does).
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

    assert_eq!(
        std::fs::metadata(&virt).map(|m| m.len()).ok(),
        Some(PAYLOAD_LEN),
        "std::fs::metadata could not see the virtual file, or reported the wrong size"
    );
    let (st, _) = nt_query_attributes_abs(&nt);
    assert!(
        st >= 0,
        "NtQueryAttributesFile could not see the virtual file: {st:#x}"
    );
    let (st, size) = nt_query_full_attributes_abs(&nt);
    assert!(
        st >= 0,
        "NtQueryFullAttributesFile could not see the virtual file: {st:#x}"
    );
    assert_eq!(
        size, PAYLOAD_LEN as i64,
        "NtQueryFullAttributesFile reported the wrong size"
    );
    for class in SIZED_CLASSES {
        if !class_supported(class) {
            continue; // this class does not answer for a plain real file here
        }
        if let Some((st, size)) = nt_query_by_name_abs(&nt, class) {
            assert!(
                st >= 0,
                "NtQueryInformationByName({class}) could not see the virtual file"
            );
            assert_eq!(
                size, PAYLOAD_LEN as i64,
                "NtQueryInformationByName({class}) reported the wrong size"
            );
        }
    }

    // ── a real file under the root that the director does not serve ────────
    let hidden = root.join("hidden.esp");
    let hidden_nt = format!(r"\??\{}", hidden.display());

    assert!(
        std::fs::metadata(&hidden).is_err(),
        "std::fs::metadata revealed a real, unserved file under the root"
    );
    let (st, _) = nt_query_attributes_abs(&hidden_nt);
    assert!(
        st < 0,
        "NtQueryAttributesFile revealed a real, unserved file under the root"
    );
    let (st, _) = nt_query_full_attributes_abs(&hidden_nt);
    assert!(
        st < 0,
        "NtQueryFullAttributesFile revealed a real, unserved file under the root"
    );
    for class in SIZED_CLASSES {
        if let Some((st, _)) = nt_query_by_name_abs(&hidden_nt, class) {
            assert!(
                st < 0,
                "NtQueryInformationByName({class}) revealed a real, unserved file under the root"
            );
        }
    }

    // ── a name that never existed ───────────────────────────────────────────
    let absent_nt = format!(r"\??\{}", root.join("absent.esm").display());
    let (st, _) = nt_query_full_attributes_abs(&absent_nt);
    assert!(
        st < 0,
        "a name in neither the VFS nor on disk reported success"
    );
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
