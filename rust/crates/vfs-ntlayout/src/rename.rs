//! `FILE_RENAME_INFORMATION` (and `_EX`) parsing.

/// The target of a rename, as the caller wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameTarget {
    /// `RootDirectory`: 0 for an absolute target, otherwise a handle the name is relative to.
    pub root_dir: usize,
    /// `FileName`, decoded lossily from UTF-16.
    pub name: String,
}

/// Parse a `FILE_RENAME_INFORMATION` buffer: `ReplaceIfExists` padded to a pointer at 0,
/// `RootDirectory` (a handle) after it, then `FileNameLength` (bytes) and `FileName`: at 8, 16
/// and 20 in a 64-bit process, 4, 8 and 12 in a 32-bit one. `None` for a buffer too short for
/// the header or for the name it declares.
pub fn parse_rename_info(b: &[u8]) -> Option<RenameTarget> {
    parse_rename_info_for(b, crate::PTR)
}

/// [`parse_rename_info`] for pointers of `ptr` bytes (8 or 4).
pub fn parse_rename_info_for(b: &[u8], ptr: usize) -> Option<RenameTarget> {
    let len_at = 2 * ptr;
    let name_at = len_at + 4;
    if b.len() < name_at {
        return None;
    }
    let mut handle = [0u8; 8];
    handle[..ptr].copy_from_slice(b.get(ptr..len_at)?);
    let root_dir = u64::from_le_bytes(handle) as usize;
    let namelen = u32::from_le_bytes(b[len_at..name_at].try_into().ok()?) as usize;
    let end = name_at.checked_add(namelen)?;
    let raw = b.get(name_at..end)?;
    let units: Vec<u16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    Some(RenameTarget {
        root_dir,
        name: String::from_utf16_lossy(&units),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rename_info(root_dir: usize, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let namelen = units.len() * 2;
        let mut buf = vec![0u8; 20 + namelen];
        buf[8..16].copy_from_slice(&root_dir.to_le_bytes());
        buf[16..20].copy_from_slice(&(namelen as u32).to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            buf[20 + i * 2..22 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        buf
    }

    #[test]
    fn an_absolute_target_has_no_root_directory() {
        let t = parse_rename_info(&rename_info(0, r"\??\C:\root\new.esp")).unwrap();
        assert_eq!(t.root_dir, 0);
        assert_eq!(t.name, r"\??\C:\root\new.esp");
    }

    #[test]
    fn a_relative_target_carries_its_root_directory() {
        let t = parse_rename_info(&rename_info(0x4321, "new.esp")).unwrap();
        assert_eq!((t.root_dir, t.name.as_str()), (0x4321, "new.esp"));
    }

    /// The same structure as a 32-bit (WOW64) process lays it out: the handle at 4, the
    /// length at 8, the name at 12.
    #[test]
    fn a_32_bit_layout_is_read_at_its_own_offsets() {
        let units: Vec<u16> = "new.esp".encode_utf16().collect();
        let mut b = vec![0u8; 12 + units.len() * 2];
        b[4..8].copy_from_slice(&0x4321u32.to_le_bytes());
        b[8..12].copy_from_slice(&((units.len() * 2) as u32).to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            b[12 + i * 2..14 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        let t = parse_rename_info_for(&b, 4).unwrap();
        assert_eq!((t.root_dir, t.name.as_str()), (0x4321, "new.esp"));
        assert_eq!(parse_rename_info_for(&[0u8; 11], 4), None);
    }

    #[test]
    fn a_short_header_or_a_name_past_the_end_is_declined() {
        assert_eq!(parse_rename_info(&[0u8; 19]), None);
        let mut b = rename_info(0, "abc");
        b.truncate(b.len() - 1);
        assert_eq!(parse_rename_info(&b), None);
        let mut huge = rename_info(0, "a");
        huge[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_rename_info(&huge), None);
    }

    #[test]
    fn an_empty_name_is_an_empty_string() {
        assert_eq!(parse_rename_info(&rename_info(0, "")).unwrap().name, "");
    }
}
