//! Composition backends (ported from Clojure `router` / `layered` / overlay reads).
//!
//! Full CoW write path (copy-up on first write) is partial: read-side whiteouts
//! and upper-wins are implemented; create/write-through is M-Write follow-up.

mod casefold;
mod glob;
mod inline;
mod layered;
mod memory;
mod overlay;
mod readonly;
mod router;
mod seekable;
mod subdir;

pub use inline::InlineProvider;
pub use layered::LayeredProvider;
pub use memory::MemoryProvider;
pub use overlay::OverlayProvider;
pub use readonly::ReadOnlyProvider;
pub use router::{Route, RouterProvider};
pub use seekable::SeekableProvider;
pub use subdir::SubdirProvider;

use std::collections::HashMap;
use std::sync::Arc;
use vfs_provider::{DirEntry, Provider};

/// The entries of a merged directory listing, ordered by folded name.
///
/// `by_folded` maps `vfs_core::fold(entry.name)` to the entry — the map every
/// merging `readdir` here (and `vfs-director`'s `MountGraph`) already builds
/// to make one spelling win. Sorting on those keys gives the order
/// `sort_by_key(|e| fold(&e.name))` gives, without folding two names on every
/// comparison: that cost 33 ms for a 3,000-entry directory. The keys are
/// distinct, so the order is total and no tie-break is involved.
pub fn sorted_by_folded_name(by_folded: HashMap<String, DirEntry>) -> Vec<DirEntry> {
    let mut keyed: Vec<(String, DirEntry)> = by_folded.into_iter().collect();
    keyed.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    keyed.into_iter().map(|(_, e)| e).collect()
}

/// The stored spelling of the last component of `p` in `provider`, or `None`
/// if it has no such entry: [`Provider::stored_name`], and for a provider
/// that does not implement it, the matching name from a listing of the
/// parent.
///
/// The provider's root has no name of its own; it answers `None`.
pub fn stored_name(provider: &dyn Provider, p: vfs_provider::VPath) -> Result<Option<String>, i32> {
    match provider.stored_name(p) {
        Err(e) if e == vfs_provider::not_supported() => {}
        answered => return answered,
    }
    let rel = p.rel.trim_matches('/');
    if rel.is_empty() {
        return Ok(None);
    }
    let (parent, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    let entries = match provider.readdir(vfs_provider::VPath::new(p.root, parent)) {
        Ok(entries) => entries,
        Err(e) if e == vfs_provider::not_found() || e == vfs_provider::not_a_dir() => {
            return Ok(None)
        }
        Err(e) => return Err(e),
    };
    let folded = vfs_core::fold(name);
    Ok(entries
        .into_iter()
        .map(|e| e.name)
        .find(|n| vfs_core::fold(n) == folded))
}

/// Stack providers bottom→top so the last entry wins on conflicts (layer order).
///
/// Empty input is rejected. A single entry is returned as-is.
pub fn stack_layers(
    layers_bottom_to_top: Vec<Arc<dyn Provider>>,
) -> Result<Arc<dyn Provider>, &'static str> {
    if layers_bottom_to_top.is_empty() {
        return Err("stack_layers: empty");
    }
    let mut iter = layers_bottom_to_top.into_iter();
    let mut acc = iter.next().unwrap();
    for upper in iter {
        acc = Arc::new(LayeredProvider::new(upper, acc));
    }
    Ok(acc)
}

#[cfg(test)]
mod stack_tests {
    use super::*;
    use vfs_provider::{VPath, OPEN_READ};

    #[test]
    fn sorted_by_folded_name_matches_a_sort_on_the_folded_name() {
        use vfs_provider::{DirEntry, Stat, KIND_FILE};
        // Mixed case, non-ASCII, a name whose fold changes its length (`İ`),
        // and names that differ only past a shared prefix.
        let names = [
            "Zebra.esp",
            "apple.esp",
            "Mango.ESP",
            "ÄÖÜ.txt",
            "äpfel.txt",
            "İstanbul",
            "istanbul2",
            "a",
            "B",
            "_x",
            "Data",
            "data2",
            "DATA1",
            "meshes",
            "Meshes2",
            "é",
            "E",
            "z",
        ];
        let entry = |n: &str| DirEntry {
            name: n.to_string(),
            stat: Stat {
                kind: KIND_FILE,
                size: n.len() as u64,
                mtime: 0,
            },
        };
        let mut want: Vec<DirEntry> = names.iter().map(|n| entry(n)).collect();
        want.sort_by_key(|e| vfs_core::fold(&e.name));
        let map = names
            .iter()
            .map(|n| (vfs_core::fold(n), entry(n)))
            .collect();
        let got = sorted_by_folded_name(map);
        let names_of = |v: &[DirEntry]| v.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        assert_eq!(names_of(&got), names_of(&want));
    }

    #[test]
    fn stack_layers_rejects_empty() {
        assert!(stack_layers(vec![]).is_err());
    }

    #[test]
    fn stack_layers_top_wins_over_two_bases() {
        let bottom = Arc::new(InlineProvider::from_files([("f", b"0".as_slice())]));
        let mid = Arc::new(InlineProvider::from_files([("f", b"1".as_slice())]));
        let top = Arc::new(InlineProvider::from_files([("f", b"2".as_slice())]));
        let stacked = stack_layers(vec![bottom, mid, top]).unwrap();
        let (h, size, _) = stacked.open(VPath::at_default("f"), OPEN_READ).unwrap();
        assert_eq!(size, 1);
        let mut buf = [0u8; 4];
        let n = stacked.read_at(h, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"2");
        stacked.close(h).unwrap();
    }

    #[test]
    fn a_layered_stack_reports_the_weakest_access_of_its_children() {
        use vfs_provider::Access;
        let bottom = Arc::new(InlineProvider::from_files([("f", b"0".as_slice())]));
        let top = Arc::new(InlineProvider::from_files([("f", b"1".as_slice())]));
        let stacked = stack_layers(vec![bottom, top]).unwrap();
        assert_eq!(stacked.capabilities().access, Access::Read);
    }

    #[test]
    fn a_layered_stack_of_immutable_children_is_immutable() {
        let bottom = Arc::new(InlineProvider::from_files([("f", b"0".as_slice())]));
        let top = Arc::new(InlineProvider::from_files([("f", b"1".as_slice())]));
        let stacked = stack_layers(vec![bottom, top]).unwrap();
        assert!(
            stacked.capabilities().immutable,
            "inline content never changes"
        );
    }

    #[test]
    fn inline_provider_passes_conformance() {
        let p: Arc<dyn vfs_provider::Provider> = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES.iter().copied(),
        ));
        vfs_provider::assert_conformance(p);
    }

    #[test]
    fn a_layered_stack_passes_conformance() {
        // Bottom holds the full fixture tree, top holds nothing: the stack
        // must still present the reference tree.
        let bottom: Arc<dyn vfs_provider::Provider> = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES.iter().copied(),
        ));
        let top: Arc<dyn vfs_provider::Provider> = Arc::new(InlineProvider::from_files(
            std::iter::empty::<(&str, &[u8])>(),
        ));
        vfs_provider::assert_conformance(stack_layers(vec![bottom, top]).unwrap());
    }
}
