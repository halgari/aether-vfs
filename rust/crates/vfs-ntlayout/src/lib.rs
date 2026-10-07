//! The pure NT byte layouts and decisions the shim's hooks apply, in a crate that builds and
//! tests on any host. The hooks keep the raw-pointer plumbing (`slice::from_raw_parts_mut`, the
//! `IO_STATUS_BLOCK` write); everything that is arithmetic on bytes lives here.
#![forbid(unsafe_code)]

// `objname` writes a native-width pointer and `rename` reads a `usize` from 8 bytes; both
// assume the 64-bit NT layouts. Fail the build rather than misread on a 32-bit target.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("vfs-ntlayout encodes the 64-bit NT layouts and needs a 64-bit target");

mod dirinfo;
mod disposition;
mod info;
mod objname;
mod rename;

pub use dirinfo::*;
pub use disposition::*;
pub use info::*;
pub use objname::*;
pub use rename::*;
