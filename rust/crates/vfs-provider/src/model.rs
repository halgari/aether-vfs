//! Value types crossing the provider boundary.

/// Ops-layer file kind. Not the same encoding as `vfs-shared` snapshot kinds.
pub const KIND_FILE: u8 = 1;
pub const KIND_DIR: u8 = 2;
pub const KIND_TOMBSTONE: u8 = 3;

/// An opaque handle, scoped to the provider that issued it.
pub type Handle = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub kind: u8,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub stat: Stat,
}

/// Attribute change by path. `None` means "leave alone". `size` is present
/// because NT sets end-of-file by path as well as by handle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetAttr {
    pub mtime: Option<i64>,
    pub size: Option<u64>,
}
