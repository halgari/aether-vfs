//! The pure NT byte layouts and decisions the shim's hooks apply, in a crate that builds and
//! tests on any host. The hooks keep the raw-pointer plumbing (`slice::from_raw_parts_mut`, the
//! `IO_STATUS_BLOCK` write); everything that is arithmetic on bytes lives here.
#![forbid(unsafe_code)]

mod dirinfo;
mod disposition;
mod objname;

pub use dirinfo::*;
pub use disposition::*;
pub use objname::spoofed_object_name;
