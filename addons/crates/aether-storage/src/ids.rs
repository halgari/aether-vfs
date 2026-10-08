//! Store file ids.
//!
//! Every file in the block store belongs either to a layer or to the
//! pull-through cache, and its id says which: a layer file is `b'L'` followed by
//! its 16-byte GUID, a cache file is `b'C'` followed by the 16-byte BLAKE3
//! identity hash of the cached source file. Both are 17 bytes. Reconciliation
//! relies on the prefix to tell the two apart, and on "anything else" being
//! [`StoreIdKind::Foreign`].

/// A layer file's identity, fixed at creation and kept across renames.
pub(crate) type Guid = [u8; 16];

const LAYER_PREFIX: u8 = b'L';
const CACHE_PREFIX: u8 = b'C';

/// A fresh random GUID (UUID v4 bytes).
pub(crate) fn new_guid() -> Guid {
    *uuid::Uuid::new_v4().as_bytes()
}

/// The store file id of a layer file.
pub(crate) fn layer_file_id(g: &Guid) -> [u8; 17] {
    prefixed(LAYER_PREFIX, g)
}

/// The store file id of a cache file, from its 16-byte identity hash.
pub(crate) fn cache_file_id(h: &[u8; 16]) -> [u8; 17] {
    prefixed(CACHE_PREFIX, h)
}

fn prefixed(p: u8, rest: &[u8; 16]) -> [u8; 17] {
    let mut id = [0u8; 17];
    id[0] = p;
    id[1..].copy_from_slice(rest);
    id
}

/// What a store file id names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoreIdKind {
    /// A layer file, by GUID.
    Layer(Guid),
    /// A cache file, by identity hash.
    Cache([u8; 16]),
    /// Not an id `aether-storage` writes.
    Foreign,
}

/// Classifies a store file id by its prefix and length.
pub(crate) fn classify_store_id(id: &[u8]) -> StoreIdKind {
    let Some((&p, rest)) = id.split_first() else {
        return StoreIdKind::Foreign;
    };
    let Ok(rest) = <[u8; 16]>::try_from(rest) else {
        return StoreIdKind::Foreign;
    };
    match p {
        LAYER_PREFIX => StoreIdKind::Layer(rest),
        CACHE_PREFIX => StoreIdKind::Cache(rest),
        _ => StoreIdKind::Foreign,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_prefixed_and_classified() {
        let g = new_guid();
        assert!(matches!(classify_store_id(&layer_file_id(&g)), StoreIdKind::Layer(x) if x == g));
        assert!(
            matches!(classify_store_id(&cache_file_id(&[9; 16])), StoreIdKind::Cache(x) if x == [9; 16])
        );
        assert!(matches!(classify_store_id(b"other"), StoreIdKind::Foreign));
        assert_ne!(new_guid(), new_guid());
    }

    #[test]
    fn a_prefix_with_the_wrong_length_is_foreign() {
        assert!(matches!(classify_store_id(b""), StoreIdKind::Foreign));
        assert!(matches!(
            classify_store_id(&[b'L'; 16]),
            StoreIdKind::Foreign
        ));
        assert!(matches!(
            classify_store_id(&[b'C'; 18]),
            StoreIdKind::Foreign
        ));
        assert!(matches!(
            classify_store_id(&[b'X'; 17]),
            StoreIdKind::Foreign
        ));
    }
}
