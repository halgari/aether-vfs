#![forbid(unsafe_code)]
//! The provider contract: what a filesystem provider can do, how it is
//! addressed, and the conformance suite that holds every implementation —
//! Rust or host-language — to the same standard.

mod caps;

pub use caps::{Access, Capabilities};
