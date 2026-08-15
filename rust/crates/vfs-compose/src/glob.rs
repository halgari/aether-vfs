//! Minimal glob matching for router patterns (`*`, `**`, `?`).
//!
//! Patterns are matched against a full virtual path like `/game/a.dat` (leading
//! slash optional on either side). Matching is case-insensitive (Windows VFS).

/// Return true when `path` matches `pattern` (glob semantics).
pub fn matches(pattern: &str, path: &str) -> bool {
    let pat = normalize(pattern);
    let pth = normalize(path);
    match_segments(
        &split_segs(&pat),
        &split_segs(&pth),
    )
}

/// Folded with [`vfs_core::fold`], the same fold the shim applies to vpath
/// components before they cross the ring — a route pattern written
/// `/Data/ÜBER/**` has to match the `data/über/…` the router is actually
/// asked about.
///
/// `match_one` below still compares literals byte-by-byte, which is correct
/// once both sides are folded the same way; only `?` is byte-scoped (it
/// consumes one UTF-8 byte, not one character), unchanged by this and
/// unrelated to case.
fn normalize(s: &str) -> String {
    let s = s.replace('\\', "/");
    let s = s.trim_matches('/');
    vfs_core::fold(s)
}

fn split_segs(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split('/').filter(|p| !p.is_empty()).collect()
}

fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    match_rec(pat, 0, path, 0)
}

fn match_rec(pat: &[&str], pi: usize, path: &[&str], si: usize) -> bool {
    if pi == pat.len() {
        return si == path.len();
    }
    let p = pat[pi];
    if p == "**" {
        // Match zero or more path segments.
        if match_rec(pat, pi + 1, path, si) {
            return true;
        }
        if si < path.len() && match_rec(pat, pi, path, si + 1) {
            return true;
        }
        return false;
    }
    if si == path.len() {
        return false;
    }
    if match_one(p, path[si]) {
        return match_rec(pat, pi + 1, path, si + 1);
    }
    false
}

fn match_one(pat: &str, seg: &str) -> bool {
    if pat == "*" {
        return true;
    }
    let pb = pat.as_bytes();
    let sb = seg.as_bytes();
    let mut i = 0usize;
    let mut j = 0usize;
    let mut star = None::<(usize, usize)>;
    while j < sb.len() {
        if i < pb.len() && (pb[i] == b'?' || pb[i] == sb[j]) {
            i += 1;
            j += 1;
        } else if i < pb.len() && pb[i] == b'*' {
            star = Some((i, j));
            i += 1;
        } else if let Some((si, sj)) = star {
            i = si + 1;
            j = sj + 1;
            star = Some((si, j));
        } else {
            return false;
        }
    }
    while i < pb.len() && pb[i] == b'*' {
        i += 1;
    }
    i == pb.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_star_and_single() {
        assert!(matches("/game/**", "/game/a.dat"));
        assert!(matches("/game/**", "/game/sub/a.dat"));
        assert!(!matches("/game/**", "/windows/x"));
        assert!(matches("/game/*.exe", "/game/app.exe"));
        assert!(!matches("/game/*.exe", "/game/sub/app.exe"));
    }

    #[test]
    fn case_insensitive() {
        assert!(matches("/Game/**", "/game/A.DAT"));
    }

    #[test]
    fn case_insensitive_beyond_ascii() {
        // The shim folds with `vfs_core::fold`, so a route whose pattern has
        // a non-ASCII-cased component must still match the folded vpath the
        // router receives.
        assert!(matches("/Data/ÜBER/**", &vfs_core::fold("/Data/ÜBER/a.esp")));
        assert!(matches("/Data/Мод/*.esp", &vfs_core::fold("/Data/МОД/A.ESP")));
    }
}
