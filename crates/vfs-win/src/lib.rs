#![deny(unsafe_code)]

//! Windows platform layer: cross-process shared memory backing a `vfs_ipc::SharedSeg`.

mod mapping;

pub use mapping::SharedMapping;
