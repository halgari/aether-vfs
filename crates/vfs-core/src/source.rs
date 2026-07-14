//! Tagged encoding of a file node's `source` blob: either a raw UTF-8 disk
//! path (a path never starts with NUL) or a zip-window `[0x00][u64 LE
//! offset][container path UTF-8]`.

/// Marks a zip-window blob. A raw disk path never begins with NUL.
const ZIP_TAG: u8 = 0x00;

/// A decoded `source` blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source<'a> {
    /// Raw UTF-8 disk path (legacy / disk-backed layers).
    Disk(&'a [u8]),
    /// A contiguous window inside a Stored zip entry.
    ZipWindow { offset: u64, container: &'a [u8] },
}

/// Encode a zip-window source: `[0x00][u64 LE offset][container path bytes]`.
pub fn encode_zip_window(offset: u64, container: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(9 + container.len());
    v.push(ZIP_TAG);
    v.extend_from_slice(&offset.to_le_bytes());
    v.extend_from_slice(container.as_bytes());
    v
}

/// Decode a `source` blob. A leading NUL selects a zip-window; anything else
/// (including empty) is a raw disk path. Malformed zip-window blobs (too short)
/// fall back to `Disk` so callers stay fail-safe.
pub fn decode(blob: &[u8]) -> Source<'_> {
    if blob.first() == Some(&ZIP_TAG) && blob.len() >= 9 {
        let mut off = [0u8; 8];
        off.copy_from_slice(&blob[1..9]);
        Source::ZipWindow { offset: u64::from_le_bytes(off), container: &blob[9..] }
    } else {
        Source::Disk(blob)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_zip_window() {
        let blob = encode_zip_window(0x1_0000_0007, r"C:\GameLayers\base.zip");
        assert_eq!(
            decode(&blob),
            Source::ZipWindow { offset: 0x1_0000_0007, container: br"C:\GameLayers\base.zip" }
        );
    }

    #[test]
    fn a_plain_path_decodes_as_disk() {
        assert_eq!(decode(br"D:\Mods\Cool\foo.esp"), Source::Disk(br"D:\Mods\Cool\foo.esp"));
    }

    #[test]
    fn a_truncated_zip_blob_is_treated_as_disk() {
        // Leading NUL but fewer than 9 bytes -> not a valid window.
        assert_eq!(decode(&[0x00, 0x01, 0x02]), Source::Disk(&[0x00, 0x01, 0x02]));
    }
}
