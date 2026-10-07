//! The deletion markers an older shim wrote into the director's upper layer (`<name>.__vfs_wh__`).
//! Nothing writes them now; [`is_whiteout`] lets the shim hide the ones still on users' disks.

/// The suffix an older shim appended to a deleted file's name.
pub const WHITEOUT_SUFFIX: &str = ".__vfs_wh__";

/// If `name` is a whiteout marker, the base name it hides; else `None`.
pub fn is_whiteout(name: &str) -> Option<&str> {
    name.strip_suffix(WHITEOUT_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_names_the_file_it_hides() {
        let m = format!("foo.esp{WHITEOUT_SUFFIX}");
        assert_eq!(m, "foo.esp.__vfs_wh__");
        assert_eq!(is_whiteout(&m), Some("foo.esp"));
        assert_eq!(is_whiteout("foo.esp"), None);
    }
}
