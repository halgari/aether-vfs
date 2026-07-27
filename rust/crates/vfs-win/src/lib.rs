//! Windows platform layer: cross-process shared memory backing a `vfs_ipc::SharedSeg`.
//! Unsafe is confined to `mapping` and `event_notifier`.

mod event_notifier;
mod mapping;

pub use event_notifier::EventNotifier;
pub use mapping::SharedMapping;
