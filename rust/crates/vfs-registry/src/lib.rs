//! Registry overlay model: canonical key paths and a copy-on-write overlay tree.
//!
//! Portable on purpose (depends only on `vfs-core` for case folding) so the
//! persistence format, merged view and director logic build and test on Linux.

pub mod format;
pub mod layout;
pub mod merge;
pub mod overlay;
pub mod path;

pub use format::{FormatError, MAGIC, decode, encode};
pub use merge::{KeyView, merge};
// compat: removed by cleanup stream I
#[doc(hidden)]
pub use merge::{MergedKey, RealKey};
pub use overlay::{Child, Lookup, Node, Overlay, RegError, Value, utf16_len};
pub use path::PathError;
