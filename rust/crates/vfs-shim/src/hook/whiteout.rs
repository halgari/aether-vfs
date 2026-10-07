//! The shim's whiteout markers in a director listing.

use std::collections::HashSet;

use vfs_core::fold;
use vfs_redirect::{DirItem, is_whiteout};

/// Remove the shim's whiteout markers from a directory listing, and with each one
/// the name it hides.
///
/// The shim spelled a whiteout `<name>.__vfs_wh__` ([`vfs_redirect::WHITEOUT_SUFFIX`]);
/// `vfs_compose::OverlayProvider` spells it `.wh.<name>`. Since gate 4 Task 6
/// those two conventions shared one physical directory — a host mounts
/// `overlay_layer_dir` as the director's write layer, which is the directory the
/// shim-local overlay wrote into — and the director has no reason to hide a
/// spelling it does not use. So `client.readdir` hands the markers back as
/// ordinary zero-byte files, and before this function the listing branch passed
/// them straight to the game: a phantom `<file>.__vfs_wh__` entry *and* the file
/// it was supposed to hide, still listed. Both halves, from one marker.
///
/// The shim no longer writes markers (task C8 removed its overlay), but existing
/// write layers on users' disks still hold the ones older shims wrote, so the
/// listing keeps honouring them.
///
/// ## Why the shim never spelled whiteouts `.wh.<name>`
///
/// `vfs_compose::OverlayProvider` answers `is_whiteout` from a **cached
/// per-directory index**, licensed by its own doc comment: *"this provider is the
/// only writer of `.wh.` markers in its own upper … A process outside this
/// provider mutating the upper's markers underneath us is already outside the
/// contract."* A second writer of `.wh.` into that upper would have left a
/// listing and an open disagreeing about whether a file exists.
///
/// ## Ordering constraint at the call site
///
/// Callers must apply this **before** any wildcard filter. `*.esp` does not
/// match `gone.esp.__vfs_wh__`, so filtering first drops the marker on its own
/// and leaves `gone.esp` listed — the hiding half silently lost for exactly
/// the queries a game makes most.
///
/// Pure and in-memory: the marker names are already in the listing, so nothing
/// here touches the filesystem, and enumeration pays one pass over entries it
/// was going to copy anyway.
pub(crate) fn strip_whiteout_markers(items: Vec<DirItem>) -> Vec<DirItem> {
    // Two passes, because a marker may sort after the name it hides. A set
    // rather than a scan of a `Vec`: a mod that removes a few hundred files
    // from one directory is an ordinary thing to do, and that is the case
    // where the quadratic version would show up.
    let hidden: HashSet<String> = items
        .iter()
        .filter_map(|i| is_whiteout(&i.name).map(fold))
        .collect();
    if hidden.is_empty() {
        return items;
    }
    items
        .into_iter()
        .filter(|i| !hidden.contains(&fold(&i.name)) && is_whiteout(&i.name).is_none())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str) -> DirItem {
        DirItem {
            name: name.into(),
            is_dir: false,
            size: 0,
            mtime: 0,
        }
    }

    /// What the director hands back for a directory whose write layer holds a marker an older
    /// shim wrote: the deleted file (still in the read layers) *and* the marker. Both go, in
    /// either order and in any letter case, and unrelated entries stay.
    #[test]
    fn a_marker_and_the_name_it_hides_are_dropped_and_nothing_else_is() {
        let marker = format!("gone.esp{}", vfs_redirect::WHITEOUT_SUFFIX);
        for listing in [
            vec![item("GONE.esp"), item(&marker), item("kept.esp")],
            vec![item(&marker), item("kept.esp"), item("gone.esp")],
        ] {
            let out = strip_whiteout_markers(listing);
            let names: Vec<&str> = out.iter().map(|i| i.name.as_str()).collect();
            assert_eq!(names, ["kept.esp"], "{names:?}");
        }
        let untouched = strip_whiteout_markers(vec![item("a.esp"), item("b.esp")]);
        assert_eq!(
            untouched.len(),
            2,
            "a listing with no marker is returned as it is"
        );
    }
}
