//! Core data types.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LayerId(pub u32);

#[derive(Clone, Debug)]
pub struct Layer {
    pub id: LayerId,
    pub entries: Vec<InputEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// What a path is: an input entry's kind, and a tree node's.
pub enum EntryKind {
    File,
    Dir,
    Tombstone,
}

// compat: removed by cleanup stream I
#[doc(hidden)]
pub type NodeKind = EntryKind;

#[derive(Clone, Debug)]
pub struct InputEntry {
    pub vpath: String,
    pub kind: EntryKind,
    pub source: SourceId,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceId(pub Box<[u8]>);

impl SourceId {
    pub fn new(bytes: impl Into<Box<[u8]>>) -> Self {
        SourceId(bytes.into())
    }
}

impl From<&str> for SourceId {
    fn from(s: &str) -> Self {
        SourceId(s.as_bytes().into())
    }
}

/// A tree node's metadata. Not [`vfs_provider`]'s `Stat`, whose `kind` is a `u8`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeStat {
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: i64,
}

/// One entry of a tree directory listing; compare `vfs_provider::DirEntry`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheKey(pub [u8; 32]);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    File {
        source: SourceId,
        size: u64,
        mtime: i64,
        layer: LayerId,
        cache_key: CacheKey,
    },
    Dir,
    Tombstone,
    NotFound,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildError {
    EmptyPath,
    EscapesRoot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VfsError {
    NotADirectory,
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_id_constructors() {
        assert_eq!(SourceId::from("abc"), SourceId::new(b"abc".to_vec()));
    }

    #[test]
    fn types_are_constructible() {
        let e = InputEntry {
            vpath: "data/a.esp".into(),
            kind: EntryKind::File,
            source: "root/data/a.esp".into(),
            size: 10,
            mtime: 42,
        };
        let _layer = Layer { id: LayerId(0), entries: vec![e] };
        let _r = Resolution::NotFound;
        assert_eq!(_r, Resolution::NotFound);
    }
}
