#![deny(unsafe_code)]
//! `vfs-shared`: bitness-neutral shared-memory snapshot layout for the virtual
//! tree. Pure byte-buffer operations; the OS shared-memory mapping lives
//! elsewhere. Layout, builder and reader are unsafe-free.

pub mod layout;
pub mod builder;
pub mod reader;

#[cfg(feature = "bridge")]
pub mod bridge;

pub use builder::SnapshotBuilder;

pub use reader::{
    LayoutError, NodeKind, ReadError, SnapDirEntry, SnapResolution, SnapStat, SnapshotReader,
};
