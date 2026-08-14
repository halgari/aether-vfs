//! Windows platform layer: cross-process shared memory backing a `vfs_ipc::SharedSeg`,
//! and raw Win32 volume/path lookups for canonicalisation (`vfs-redirect`).
//! Unsafe is confined to `mapping`, `event_notifier`, and `volumes`.

mod event_notifier;
mod mapping;
mod volumes;

pub use event_notifier::EventNotifier;
pub use mapping::SharedMapping;
pub use volumes::{
    drive_mappings, expand_long_path, final_path_for_open, is_device_namespace_name,
    short_path_name, DriveMapping,
};
