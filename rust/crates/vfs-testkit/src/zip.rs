//! A one-entry Stored zip, hand-rolled on purpose.
//!
//! The tests that use it are about what the director does with an archive, and
//! a fixture archive whose exact bytes are known is what makes "the source is
//! byte-identical afterwards" a meaningful assertion. Stored only: that is the
//! one method `ZipProvider` supports.

use std::path::Path;

/// CRC-32 (IEEE, reflected), as the zip format records it.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// The bytes of a zip holding one Stored entry named `entry`.
pub fn stored_zip_bytes(entry: &str, content: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    let crc = crc32(content);
    let n = entry.len() as u16;
    let len = content.len() as u32;
    buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&n.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(entry.as_bytes());
    buf.extend_from_slice(content);
    let cd_start = buf.len() as u32;
    buf.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 6]);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&n.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&[0u8; 8]);
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(entry.as_bytes());
    let cd_size = buf.len() as u32 - cd_start;
    buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&cd_size.to_le_bytes());
    buf.extend_from_slice(&cd_start.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf
}

/// Write a one-entry Stored zip to `path`. Panics on I/O failure.
pub fn write_stored_zip(path: &Path, entry: &str, content: &[u8]) {
    std::fs::write(path, stored_zip_bytes(entry, content)).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn the_archive_has_the_three_zip_signatures_and_the_content() {
        let b = stored_zip_bytes("a/b.txt", b"hello");
        assert_eq!(&b[..4], b"PK\x03\x04");
        assert!(b.windows(4).any(|w| w == b"PK\x01\x02"));
        assert!(b.windows(4).any(|w| w == b"PK\x05\x06"));
        assert!(b.windows(5).any(|w| w == b"hello"));
    }
}
