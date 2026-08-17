//! Composition backends (ported from Clojure `router` / `layered` / overlay reads).
//!
//! Full CoW write path (copy-up on first write) is partial: read-side whiteouts
//! and upper-wins are implemented; create/write-through is M-Write follow-up.

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

use std::sync::Arc;
use vfs_provider::Provider;

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
