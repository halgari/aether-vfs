//! Case folding — single source of truth for case-insensitive comparison.

/// Lowercase simple case fold. MVP uses `char::to_lowercase` (Unicode simple
/// folding). This is the single source of truth for case-insensitive matching.
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
