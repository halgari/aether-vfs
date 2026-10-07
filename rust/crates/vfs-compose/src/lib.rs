//! Composition backends: providers built out of other providers.
//!
//! - [`OverlayProvider`]: a writable upper over a base. Reads fall through to
//!   the base; the first write to a base-only file copies the whole file up
//!   (staged as `.cu.<n>.<name>`, then renamed into place), and removing a
//!   base-visible path writes a `.wh.<name>` whiteout into the upper. The
//!   base is never mutated.
//! - [`LayeredProvider`]: top-wins layering of two providers.
//! - [`RouterProvider`]: glob-based routing to a provider per path.
//! - [`SubdirProvider`], [`SeekableProvider`] and [`ReadOnlyProvider`]: wrappers
//!   that map paths, add positional reads over a sequential provider, or
//!   demote write access.
//! - [`MemoryProvider`] (read-write) and [`InlineProvider`] (read-only): in-memory
//!   trees.

mod casefold;
mod disk;
mod glob;
mod inline;
mod layered;
mod memory;
mod mount_graph;
mod overlay;
pub mod path;
mod readonly;
mod rejected_writes;
mod router;
mod seekable;
mod subdir;

pub use disk::DiskProvider;
pub use inline::InlineProvider;
pub use layered::LayeredProvider;
pub use memory::MemoryProvider;
pub use mount_graph::MountGraph;
pub use overlay::OverlayProvider;
pub use readonly::ReadOnlyProvider;
pub use rejected_writes::{record_rejected_write, rejected_writes, reset_rejected_writes};
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

/// The merge rule for a listing with a lower and an upper side, shared by
/// `LayeredProvider` and `OverlayProvider`: the upper's entry is the live one
/// (its size, time and kind), but a name the lower side also has keeps the
/// lower side's spelling. The upper's spelling of such a name is an accident of
/// whoever wrote there first, and letting it win renames `Data` to `data` for
/// every caller.
///
/// `by_folded` holds the lower side's entries keyed by `vfs_core::fold`;
/// `key` is the fold of `upper.name`. A name only the upper has is added as
/// it is.
pub(crate) fn merge_upper_entry(
    by_folded: &mut HashMap<String, DirEntry>,
    key: String,
    upper: DirEntry,
) {
    match by_folded.entry(key) {
        std::collections::hash_map::Entry::Occupied(mut lower) => {
            lower.get_mut().stat = upper.stat;
        }
        std::collections::hash_map::Entry::Vacant(free) => {
            free.insert(upper);
        }
    }
}

/// The single-name form of [`merge_upper_entry`]: the spelling a merged
/// listing of `lower` and `upper` shows for the last component of `p` — the
/// lower's if it has the name, the upper's otherwise.
pub(crate) fn merge_stored_name(
    lower: &dyn Provider,
    upper: &dyn Provider,
    p: vfs_provider::VPath,
) -> Result<Option<String>, i32> {
    if let Some(name) = stored_name(lower, p)? {
        return Ok(Some(name));
    }
    stored_name(upper, p)
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
    let (parent, name) = vfs_core::split_parent(rel);
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

#[cfg(test)]
mod stored_name_forwarding_tests {
    use super::*;
    use vfs_provider::{Capabilities, Handle, Stat, VPath};

    /// Answers `stored_name` with a marker no listing could produce, naming the
    /// path it was asked about. A wrapper that does not forward the call falls
    /// back to the default (unsupported) and the listing, and never says this.
    struct Marker;

    impl Provider for Marker {
        fn capabilities(&self) -> Capabilities {
            Capabilities::read_only()
        }
        fn getattr(&self, _p: VPath) -> Result<Option<Stat>, i32> {
            Ok(None)
        }
        fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
            Ok(Vec::new())
        }
        fn open(&self, _p: VPath, _flags: u32) -> Result<(Handle, u64, bool), i32> {
            Err(vfs_provider::not_found())
        }
        fn close(&self, _h: Handle) -> Result<(), i32> {
            Ok(())
        }
        fn read_at(&self, _h: Handle, _o: u64, _b: &mut [u8]) -> Result<usize, i32> {
            Err(vfs_provider::bad_fh())
        }
        fn stored_name(&self, p: VPath) -> Result<Option<String>, i32> {
            Ok(Some(format!("marker:{}", p.rel)))
        }
    }

    fn ask(p: &dyn Provider, rel: &str) -> Option<String> {
        p.stored_name(VPath::at_default(rel)).expect("forwarded, not unsupported")
    }

    #[test]
    fn readonly_forwards_stored_name() {
        let p = ReadOnlyProvider::new(Arc::new(Marker));
        assert_eq!(ask(&p, "a/B").as_deref(), Some("marker:a/B"));
    }

    #[test]
    fn seekable_forwards_stored_name() {
        let p = SeekableProvider::new(Arc::new(Marker));
        assert_eq!(ask(&p, "a/B").as_deref(), Some("marker:a/B"));
    }

    #[test]
    fn subdir_forwards_stored_name_under_its_prefix() {
        let p = SubdirProvider::new(Arc::new(Marker), "root");
        assert_eq!(ask(&p, "a/B").as_deref(), Some("marker:root/a/B"));
    }

    #[test]
    fn router_forwards_stored_name_to_the_routed_provider() {
        struct Other;
        impl Provider for Other {
            fn capabilities(&self) -> Capabilities {
                Capabilities::read_only()
            }
            fn getattr(&self, _p: VPath) -> Result<Option<Stat>, i32> {
                Ok(None)
            }
            fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
                Ok(Vec::new())
            }
            fn open(&self, _p: VPath, _f: u32) -> Result<(Handle, u64, bool), i32> {
                Err(vfs_provider::not_found())
            }
            fn close(&self, _h: Handle) -> Result<(), i32> {
                Ok(())
            }
            fn read_at(&self, _h: Handle, _o: u64, _b: &mut [u8]) -> Result<usize, i32> {
                Err(vfs_provider::bad_fh())
            }
            fn stored_name(&self, _p: VPath) -> Result<Option<String>, i32> {
                Ok(Some("other".to_string()))
            }
        }
        let p = RouterProvider::new(
            Arc::new(Marker),
            vec![Route {
                pattern: "special/**".to_string(),
                provider: Arc::new(Other),
            }],
        );
        assert_eq!(ask(&p, "plain/X").as_deref(), Some("marker:plain/X"));
        assert_eq!(ask(&p, "special/X").as_deref(), Some("other"));
    }
}
