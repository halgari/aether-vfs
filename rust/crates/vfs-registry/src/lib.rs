//! Registry overlay model: canonical key paths and a copy-on-write overlay tree.
//!
//! Portable on purpose (depends only on `vfs-core` for case folding) so the
//! persistence format, merged view and director logic build and test on Linux.

pub mod overlay;
pub mod path;

pub use overlay::{Child, Lookup, Node, Overlay, RegError, Value};
pub use path::PathError;
