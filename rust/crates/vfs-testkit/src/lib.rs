//! Helpers the workspace's integration tests used to each carry a private
//! copy of. Depend on it from `[dev-dependencies]` only; nothing ships it.

pub mod artifacts;
pub mod scratch;
pub mod zip;

pub use scratch::{scratch_dir, scratch_path, scratch_root, tempdir, use_scratch_as_tmpdir, Scratch};
