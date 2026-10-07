//! NT string conventions: counted UTF-16, the `\??\` prefix, volume-relative names.

/// Decode a length-counted UTF-16 buffer (a `UNICODE_STRING` body) to a `String`.
/// Lossy: unpaired surrogates become U+FFFD rather than panicking.
pub fn utf16_to_string(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

/// Why a `UNICODE_STRING` header cannot be read as a counted UTF-16 string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountedErr {
    /// `Length` is odd. NT rejects it (`STATUS_OBJECT_NAME_INVALID`) rather than dropping the
    /// last byte.
    OddLength,
    /// `Buffer` is NULL while `Length` says there are characters (`STATUS_ACCESS_VIOLATION`).
    NullBuffer,
}

/// The number of UTF-16 units a `UNICODE_STRING` with this `Length` (in bytes) and
/// buffer-nullness holds, by NT's rules: an odd length is invalid, a zero length is the empty
/// string whether or not `Buffer` is NULL, and a NULL `Buffer` with a non-zero length is a bad
/// pointer. The odd-length check comes first, as the kernel's capture does it before probing.
pub fn counted_units(length_bytes: u16, buffer_is_null: bool) -> Result<usize, CountedErr> {
    if length_bytes & 1 != 0 {
        return Err(CountedErr::OddLength);
    }
    if length_bytes == 0 {
        return Ok(0);
    }
    if buffer_is_null {
        return Err(CountedErr::NullBuffer);
    }
    Ok(length_bytes as usize / 2)
}

/// Wrap a Win32 absolute path as an NT DOS-device path (`\??\...`). A path that
/// already carries an NT/DOS long prefix is returned unchanged.
pub fn to_nt(path: &str) -> String {
    if path.starts_with(r"\??\") || path.starts_with(r"\\?\") {
        path.to_string()
    } else {
        format!(r"\??\{path}")
    }
}

/// Strip a `\??\` / `\\?\` prefix and a leading `X:` drive, yielding the
/// volume-relative path (`\...`, no drive) that `FILE_NAME_INFORMATION` carries.
/// Idempotent on already-relative input.
pub fn nt_to_volume_relative(nt_path: &str) -> String {
    let s = nt_path
        .strip_prefix(r"\??\")
        .or_else(|| nt_path.strip_prefix(r"\\?\"))
        .unwrap_or(nt_path);
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        s[2..].to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_round_trips() {
        let s = "C:\\Games\\Skyrim\\Data\\foo.esp";
        let units: Vec<u16> = s.encode_utf16().collect();
        assert_eq!(utf16_to_string(&units), s);
    }

    #[test]
    fn counted_units_follows_nt_rules() {
        // Even length, buffer present: the unit count.
        assert_eq!(counted_units(8, false), Ok(4));
        // Zero length is the empty string, with or without a buffer.
        assert_eq!(counted_units(0, false), Ok(0));
        assert_eq!(counted_units(0, true), Ok(0));
        // An odd length is invalid, never rounded down; it is checked before the buffer.
        assert_eq!(counted_units(7, false), Err(CountedErr::OddLength));
        assert_eq!(counted_units(1, true), Err(CountedErr::OddLength));
        // A NULL buffer with characters promised is a bad pointer.
        assert_eq!(counted_units(2, true), Err(CountedErr::NullBuffer));
        assert_eq!(
            counted_units(u16::MAX - 1, true),
            Err(CountedErr::NullBuffer)
        );
        assert_eq!(counted_units(u16::MAX, false), Err(CountedErr::OddLength));
    }

    #[test]
    fn utf16_lossy_does_not_panic_on_unpaired_surrogate() {
        let units: [u16; 2] = [0xD800, b'x' as u16]; // lone high surrogate
        let _ = utf16_to_string(&units); // must not panic
    }

    #[test]
    fn volume_relative_strips_prefix_and_drive() {
        assert_eq!(
            nt_to_volume_relative(r"\??\C:\Games\Skyrim\Data\foo.esp"),
            r"\Games\Skyrim\Data\foo.esp"
        );
        assert_eq!(nt_to_volume_relative(r"\\?\D:\Mods\x.esp"), r"\Mods\x.esp");
        assert_eq!(
            nt_to_volume_relative(r"\Games\already.esp"),
            r"\Games\already.esp"
        );
    }

    #[test]
    fn to_nt_prefixes_and_preserves() {
        assert_eq!(to_nt(r"C:\overlay\foo.esp"), r"\??\C:\overlay\foo.esp");
        assert_eq!(to_nt(r"\??\C:\x"), r"\??\C:\x");
        assert_eq!(to_nt(r"\\?\C:\x"), r"\\?\C:\x");
    }
}
