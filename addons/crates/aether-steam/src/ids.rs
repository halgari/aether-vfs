//! This crate's own Steam identifiers. No steamroom type appears in this crate's
//! public API, so the dependency can be swapped or vendored without breaking
//! callers.
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AppId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DepotId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ManifestId(pub u64);

/// A chunk's id: the SHA-1 of its decompressed content.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChunkId(pub [u8; 20]);

/// A depot's AES-256 content key. `Debug` never prints the bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct DepotKey(pub [u8; 32]);

/// Skyrim Special Edition's Steam app id.
pub const SKYRIM_SE: AppId = AppId(489830);

macro_rules! display_inner {
    ($($t:ty),*) => {$(
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    )*};
}
display_inner!(AppId, DepotId, ManifestId);

impl fmt::Display for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChunkId({self})")
    }
}

impl fmt::Debug for DepotKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DepotKey(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_id_is_lowercase_hex() {
        let mut id = [0u8; 20];
        id[0] = 0xAB;
        id[19] = 0x01;
        assert_eq!(
            ChunkId(id).to_string(),
            "ab00000000000000000000000000000000000001"
        );
    }

    #[test]
    fn depot_key_debug_is_redacted() {
        assert_eq!(format!("{:?}", DepotKey([7; 32])), "DepotKey(<redacted>)");
    }
}
