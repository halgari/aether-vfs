//! The pure NT byte layouts and decisions the shim's hooks apply, in a crate that builds and
//! tests on any host. The hooks keep the raw-pointer plumbing (`slice::from_raw_parts_mut`, the
//! `IO_STATUS_BLOCK` write); everything that is arithmetic on bytes lives here.
#![forbid(unsafe_code)]

// `objname` and `rename` are the layouts with a pointer in them: each takes the pointer width
// (`*_for(…, ptr)`), the plain names using the target's, so both widths are tested on any host.
#[cfg(not(any(target_pointer_width = "64", target_pointer_width = "32")))]
compile_error!("vfs-ntlayout encodes the 32- and 64-bit NT layouts only");

/// The target's pointer width in bytes: 8, or 4 in a 32-bit (WOW64) process.
pub const PTR: usize = core::mem::size_of::<usize>();

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
