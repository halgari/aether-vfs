//! Composition backends (ported from Clojure `router` / `layered` / overlay reads).
//!
//! Full CoW write path (copy-up on first write) is partial: read-side whiteouts
//! and upper-wins are implemented; create/write-through is M-Write follow-up.

mod glob;
mod inline;
mod layered;
mod overlay;
mod router;
mod strip_prefix;

pub use inline::InlineBackend;
pub use layered::LayeredBackend;
pub use overlay::OverlayBackend;
pub use router::{Route, RouterBackend};
pub use strip_prefix::StripPrefixBackend;

use std::sync::Arc;
use vfs_protocol::Backend;

/// Stack backends bottom→top so the last entry wins on conflicts (layer order).
///
/// Empty input is rejected. A single entry is returned as-is.
pub fn stack_layers(layers_bottom_to_top: Vec<Arc<dyn Backend>>) -> Result<Arc<dyn Backend>, &'static str> {
    if layers_bottom_to_top.is_empty() {
        return Err("stack_layers: empty");
    }
    let mut iter = layers_bottom_to_top.into_iter();
    let mut acc = iter.next().unwrap();
    for upper in iter {
        acc = Arc::new(LayeredBackend::new(upper, acc));
    }
    Ok(acc)
}

#[cfg(test)]
mod stack_tests {
    use super::*;
    use vfs_protocol::OPEN_READ;

    #[test]
    fn stack_layers_rejects_empty() {
        assert!(stack_layers(vec![]).is_err());
    }

    #[test]
    fn stack_layers_top_wins_over_two_bases() {
        let bottom = Arc::new(InlineBackend::from_files([("f", b"0".as_slice())]));
        let mid = Arc::new(InlineBackend::from_files([("f", b"1".as_slice())]));
        let top = Arc::new(InlineBackend::from_files([("f", b"2".as_slice())]));
        let stacked = stack_layers(vec![bottom, mid, top]).unwrap();
        let (h, size, _) = stacked.open("f", OPEN_READ).unwrap();
        assert_eq!(size, 1);
        let mut buf = [0u8; 4];
        let n = stacked.read(h, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"2");
        stacked.release(h).unwrap();
    }
}
