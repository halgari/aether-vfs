//! DOS wildcard matching.
use crate::casefold::fold;

/// Case-insensitive DOS wildcard match. Supports `*`, `?`, and the DOS
/// meta-characters `<` (DOS_STAR ≈ `*` for MVP), `>` (DOS_QM), `"` (DOS_DOT).
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = fold(pattern).chars().collect();
    let n: Vec<char> = fold(name).chars().collect();
    do_match(&p, 0, &n, 0)
}

fn do_match(p: &[char], mut pi: usize, n: &[char], mut ni: usize) -> bool {
    while pi < p.len() {
        match p[pi] {
            '*' | '<' => {
                // Zero-or-more: try to match the remainder at each position.
                if do_match(p, pi + 1, n, ni) {
                    return true;
                }
                if ni < n.len() {
                    ni += 1;
                    continue; // stay on the star, having consumed one name char
                }
                return false;
            }
            '?' => {
                if ni >= n.len() {
                    return false;
                }
                ni += 1;
                pi += 1;
            }
            '>' => {
                // DOS_QM: one non-dot char, else zero at end / before a dot.
                if ni < n.len() && n[ni] != '.' {
                    ni += 1;
                }
                pi += 1;
            }
            '"' => {
                // DOS_DOT: a literal '.', else zero at end / before a non-dot.
                if ni < n.len() && n[ni] == '.' {
                    ni += 1;
                }
                pi += 1;
            }
            c => {
                if ni >= n.len() || n[ni] != c {
                    return false;
                }
                ni += 1;
                pi += 1;
            }
        }
    }
    ni == n.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_and_question() {
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("*.txt", "readme.txt"));
        assert!(!wildcard_match("*.txt", "readme.md"));
        assert!(!wildcard_match("*.txt", "readme"));
        assert!(wildcard_match("a?c", "abc"));
        assert!(!wildcard_match("a?c", "ac"));
    }

    #[test]
    fn case_insensitive() {
        assert!(wildcard_match("FOO*", "foobar"));
        assert!(wildcard_match("*.ESP", "Skyrim.esp"));
    }

    #[test]
    fn literal_dot_is_literal_in_core() {
        // Win32 `*.*`→match-all conversion is a shim concern; core is literal.
        assert!(wildcard_match("*.*", "foo.txt"));
        assert!(!wildcard_match("*.*", "foo"));
    }

    #[test]
    fn dos_qm_matches_zero_at_end() {
        // '>' matches one non-dot char or zero at end / before a dot.
        assert!(wildcard_match("a>", "a"));
        assert!(wildcard_match("a>", "ab"));
        assert!(!wildcard_match("a>", "a.b")); // '>' won't consume the dot, trailing ".b" remains
    }

    #[test]
    fn dos_dot_matches_period() {
        // '"' matches a literal '.' or zero at end / before non-dot.
        assert!(wildcard_match("a\"", "a."));
        assert!(wildcard_match("a\"", "a"));
    }
}
