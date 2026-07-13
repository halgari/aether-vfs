#![deny(unsafe_code)]
//! `vfs-shared`: bitness-neutral shared-memory snapshot layout for the virtual
//! tree. Pure byte-buffer operations; the OS shared-memory mapping lives
//! elsewhere. Layout/builder/reader are unsafe-free; the seqlock has one audited
//! atomic view.

pub mod layout;
pub mod builder;
pub mod reader;
pub mod seqlock;

#[cfg(feature = "bridge")]
pub mod bridge;

pub use builder::SnapshotBuilder;

pub use reader::{
    LayoutError, NodeKind, ReadError, SnapDirEntry, SnapResolution, SnapStat, SnapshotReader,
};

pub use seqlock::{publish, read_stable, AlignedBuf, PublishError};

// pub use lines are added by later tasks as items land.
