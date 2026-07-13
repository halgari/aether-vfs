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

// pub use lines are added by later tasks as items land.
