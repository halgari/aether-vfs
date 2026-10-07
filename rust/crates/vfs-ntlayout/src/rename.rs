//! `FILE_RENAME_INFORMATION` (and `_EX`) parsing.

/// The target of a rename, as the caller wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameTarget {
    /// `RootDirectory`: 0 for an absolute target, otherwise a handle the name is relative to.
    pub root_dir: usize,
    /// `FileName`, decoded lossily from UTF-16.
    pub name: String,
}

/// Parse a `FILE_RENAME_INFORMATION` buffer: `ReplaceIfExists` and padding at 0, `RootDirectory`
/// at 8, `FileNameLength` (bytes) at 16, `FileName` at 20. `None` for a buffer too short for the
/// header or for the name it declares.
pub fn parse_rename_info(b: &[u8]) -> Option<RenameTarget> {
    if b.len() < 20 {
        return None;
    }
    let root_dir = usize::from_le_bytes(b[8..16].try_into().ok()?);
    let namelen = u32::from_le_bytes(b[16..20].try_into().ok()?) as usize;
    let end = 20usize.checked_add(namelen)?;
    let raw = b.get(20..end)?;
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
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
