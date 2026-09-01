//! Fold-aware path-component helpers shared by [`crate::inline`] and
//! [`crate::memory`].
//!
//! Both providers need to answer "does this stored key lie under this
//! (possibly differently-cased) directory query", and both need the answer
//! in the key's own unfolded spelling — `fold` is not length-preserving
//! (`İ` U+0130 is two bytes, folds to three), so a folded query can never be
//! sliced off an unfolded key by byte offset. It has to be walked
//! component by component instead. One implementation, so a caller with a
//! fold-offset bug fixes it here and both providers get the fix.

/// Folded `/`-separated components of `path`. Empty for the root.
pub(crate) fn fold_components(path: &str) -> Vec<String> {
    if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').map(vfs_core::fold).collect()
    }
}

/// If `key`'s leading components fold-match every one of `query`'s (with at
/// least one of `key`'s components remaining after them), the remaining
/// components of `key` — in `key`'s own, unfolded spelling. `None` if `key`
/// is not under `query`.
///
/// Never sliced by byte offset: fold is not length-preserving (`İ` is two
/// bytes and folds to three), so lining up a folded query against an
/// unfolded key only works by walking `/`-separated parts.
pub(crate) fn fold_strip_prefix<'k>(key: &'k str, query: &[String]) -> Option<&'k str> {
    let kc: Vec<&str> = key.split('/').collect();
    if kc.len() <= query.len() {
        return None;
    }
    let matches = kc[..query.len()].iter().zip(query).all(|(c, q)| vfs_core::fold(c) == *q);
    if !matches {
        return None;
    }
    // Recover the byte offset of the remainder from key's own components,
    // not from query — see the length-preservation note above.
    let consumed: usize = kc[..query.len()].iter().map(|c| c.len() + 1).sum();
    Some(&key[consumed..])
}
