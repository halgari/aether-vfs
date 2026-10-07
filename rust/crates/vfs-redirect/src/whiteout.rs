//! The overlay's deletion markers.

/// The whiteout marker suffix appended to a deleted file's name in the overlay.
pub const WHITEOUT_SUFFIX: &str = ".__vfs_wh__";

/// The overlay marker filename that hides `name` (a deletion tombstone on disk).
pub fn whiteout_marker(name: &str) -> String {
    format!("{name}{WHITEOUT_SUFFIX}")
}

/// If `name` is a whiteout marker, the base name it hides; else `None`.
pub fn is_whiteout(name: &str) -> Option<&str> {
    name.strip_suffix(WHITEOUT_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whiteout_marker_round_trips() {
        let m = whiteout_marker("foo.esp");
        assert_eq!(m, "foo.esp.__vfs_wh__");
        assert_eq!(is_whiteout(&m), Some("foo.esp"));
        assert_eq!(is_whiteout("foo.esp"), None);
    }
}
