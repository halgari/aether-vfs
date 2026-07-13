//! Bootstrap glue: a tiny config codec and a config-file entry point used by the
//! injected DLL to build an `Engine` and install the hook. No `unsafe` here.

/// Encode `[u32 LE root_len][root utf8][snapshot bytes]`.
pub fn encode_config(root: &str, snapshot: &[u8]) -> Vec<u8> {
    let root = root.as_bytes();
    let mut out = Vec::with_capacity(4 + root.len() + snapshot.len());
    out.extend_from_slice(&(root.len() as u32).to_le_bytes());
    out.extend_from_slice(root);
    out.extend_from_slice(snapshot);
    out
}

/// Decode a buffer produced by [`encode_config`]. Returns `None` on truncation or
/// invalid UTF-8 in the root. Never panics.
pub fn decode_config(bytes: &[u8]) -> Option<(String, Vec<u8>)> {
    if bytes.len() < 4 {
        return None;
    }
    let root_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let root_end = 4usize.checked_add(root_len)?;
    if bytes.len() < root_end {
        return None;
    }
    let root = std::str::from_utf8(&bytes[4..root_end]).ok()?.to_string();
    let snapshot = bytes[root_end..].to_vec();
    Some((root, snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips() {
        let snapshot = vec![1u8, 2, 3, 4, 5];
        let bytes = encode_config(r"\??\C:\Games\Skyrim", &snapshot);
        let (root, snap) = decode_config(&bytes).unwrap();
        assert_eq!(root, r"\??\C:\Games\Skyrim");
        assert_eq!(snap, snapshot);
    }

    #[test]
    fn decode_rejects_truncated() {
        // Claims root_len = 100 but no bytes follow.
        let mut bytes = 100u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"short");
        assert!(decode_config(&bytes).is_none());
    }

    #[test]
    fn decode_rejects_too_short_for_header() {
        assert!(decode_config(&[0u8, 1]).is_none());
    }
}
