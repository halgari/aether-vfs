//! Windows platform layer: cross-process shared memory backing a `vfs_ipc::SharedSeg`.
//! Unsafe is confined to `mapping`, `event_notifier`, and `process`.

mod event_notifier;
mod mapping;
mod process;

pub use event_notifier::EventNotifier;
pub use mapping::SharedMapping;
pub use process::ProcessVm;
