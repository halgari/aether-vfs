//! Case folding — single source of truth for case-insensitive comparison.

/// Lowercase simple case fold. MVP uses `char::to_lowercase` (Unicode simple
/// folding). This is the single source of truth for case-insensitive matching.
///
/// It is load-bearing across the ring, not merely a convention. The shim folds
/// every vpath component with this function before the vpath is sent
/// (`vfs-redirect`'s `RootMap::match_canonical`), so everything that keys,
/// compares, or orders a name on the other side — `vfs-zip`'s `by_fold` index,
/// `vfs-director`'s mount-prefix matching and directory merges, `vfs-compose`'s
/// layered/overlay merges and glob routes — has to fold with this same
/// function. `to_ascii_lowercase` was used below the ring until the final
/// review of `feat/real-roots` found the split: `Data/ÜBER/a.esp` crossed as
/// `data/über/a.esp` and every index below was keyed `data/ÜBER/a.esp`, so the
/// file resolved to not-found. `DiskProvider` hid it, because Windows folds
/// Unicode itself.
///
/// If this function's definition ever changes, both sides move together; a
/// change here is a wire-visible change.
///
/// Two properties it does **not** have, both of which have already produced
/// bugs here:
///
/// 1. **Not length-preserving.** `İ` (U+0130) is two bytes and folds to three
///    (`i` + U+0307). Never slice a folded string by an offset measured on the
///    unfolded one — walk components instead. `strip_prefix` and
///    `mount_child_name` both did exactly that before the `feat/real-roots`
///    final review.
/// 2. **Not NTFS-case-equivalent.** That same `İ` folds to a genuinely
///    different name, not a case variant of the input. So "NTFS is
///    case-insensitive, therefore the folded spelling names the same file" is
///    **not** a sound argument, and must not be used to wave through a change
///    to any spelling that reaches the filesystem.
pub fn fold(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// Case-insensitive comparison. Fold-equal strings compare `Equal`; callers that
/// need stable output rely on a stable sort. (Directory siblings are keyed by
/// folded name, so a case-only collision can never occur among them.)
pub fn cmp_ci(a: &str, b: &str) -> std::cmp::Ordering {
    fold(a).cmp(&fold(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn folds_ascii_and_unicode() {
        assert_eq!(fold("FooBAR.ESP"), "foobar.esp");
        assert_eq!(fold("ÄÖÜ"), "äöü");
    }

    #[test]
    fn cmp_is_case_insensitive() {
        assert_eq!(cmp_ci("apple", "APPLE"), Ordering::Equal);
        assert_eq!(cmp_ci("Apple", "banana"), Ordering::Less);
        assert_eq!(cmp_ci("Banana", "apple"), Ordering::Greater);
    }

    #[test]
    fn cmp_ascending_not_reverse() {
        // Regression guard for the USVFS reverse-alphabetical bug.
        let mut v = vec!["Zebra", "apple", "Mango"];
        v.sort_by(|a, b| cmp_ci(a, b));
        assert_eq!(v, vec!["apple", "Mango", "Zebra"]);
    }

    #[test]
    fn cmp_fold_equal_is_equal() {
        // Names differing only by case fold-compare Equal. (They can never be
        // directory siblings, since children are keyed by folded name.)
        assert_eq!(cmp_ci("abc", "abc"), Ordering::Equal);
        assert_eq!(cmp_ci("ABC", "abc"), Ordering::Equal);
    }
}
