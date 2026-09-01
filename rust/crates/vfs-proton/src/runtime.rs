use std::cmp::Ordering;
use std::io;
use std::path::Path;

use crate::layout::Root;

/// Why a directory did not pass the GE-Proton gate.
///
/// `PROTONPATH` defaults to UMU-Proton (stock Valve Proton) whenever it is
/// unset or points somewhere wrong, so every runtime this crate hands back
/// must pass through here. A non-GE runtime is always an error, never a
/// warning.
#[derive(Debug)]
pub enum VerifyError {
    /// The runtime directory has no `version` file at all.
    Missing,
    /// The `version` file exists but could not be read.
    Unreadable(io::Error),
    /// The `version` file names a build that is not GE-Proton. Carries the
    /// file's trimmed contents so the caller can show what it actually got.
    NotGe(String),
}

/// Reads `dir/version`, confirms it names a GE-Proton build, and returns the
/// tag (e.g. `"GE-Proton11-6"`).
///
/// The file is whitespace-separated tokens; the token that starts with
/// `GE-Proton` is the tag. If no such token exists, the whole (trimmed) file
/// is returned in [`VerifyError::NotGe`] so the caller can report exactly
/// what was rejected.
pub fn verify_ge(dir: &Path) -> Result<String, VerifyError> {
    let path = dir.join("version");
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(VerifyError::Missing),
        Err(e) => return Err(VerifyError::Unreadable(e)),
    };
    let trimmed = contents.trim();
    match trimmed.split_whitespace().find(|tok| tok.starts_with("GE-Proton")) {
        Some(tag) => Ok(tag.to_string()),
        None => Err(VerifyError::NotGe(trimmed.to_string())),
    }
}

/// Orders two `GE-ProtonN-M` tags numerically by `(N, M)`, not lexically:
/// `"GE-Proton11-6"` must sort after `"GE-Proton9-1"`, which string
/// comparison gets backwards. Tags that don't parse as `GE-ProtonN-M` sort
/// below every tag that does, so junk never wins a "newest" selection.
pub fn cmp_tags(a: &str, b: &str) -> Ordering {
    match (parse_tag(a), parse_tag(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

fn parse_tag(tag: &str) -> Option<(u64, u64)> {
    let rest = tag.strip_prefix("GE-Proton")?;
    let (n, m) = rest.split_once('-')?;
    Some((n.parse().ok()?, m.parse().ok()?))
}

/// Lists the tags of every installed, verified GE-Proton runtime under
/// `root.runtimes()`, newest first. Entries that fail [`verify_ge`] — wrong
/// runtime, or a half-extracted directory with no `version` file yet — are
/// silently excluded rather than surfaced as errors, since a stray
/// non-runtime directory there is expected, not exceptional.
pub fn installed(root: &Root) -> io::Result<Vec<String>> {
    let mut tags = Vec::new();
    let entries = match std::fs::read_dir(root.runtimes()) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(tags),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Ok(tag) = verify_ge(&entry.path()) {
            tags.push(tag);
        }
    }
    tags.sort_by(|a, b| cmp_tags(a, b).reverse());
    Ok(tags)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir()
            .join(format!("vfs-proton-rt-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn verify_ge_accepts_a_real_ge_version_file() {
        // Exactly the bytes GE-Proton11-6 ships: a build id, a space, the tag.
        let d = tmpdir("ge");
        std::fs::write(d.join("version"), "1787951532 GE-Proton11-6\n").unwrap();
        assert_eq!(verify_ge(&d).unwrap(), "GE-Proton11-6");
    }

    #[test]
    fn verify_ge_rejects_stock_proton() {
        // The failure that matters: PROTONPATH defaults to UMU-Proton, which is
        // stock Valve Proton. Accepting it silently is the whole hazard.
        let d = tmpdir("stock");
        std::fs::write(d.join("version"), "1234567890 proton-9.0-4\n").unwrap();
        match verify_ge(&d) {
            Err(VerifyError::NotGe(s)) => assert!(s.contains("proton-9.0-4")),
            other => panic!("stock Proton must be rejected, got {other:?}"),
        }
    }

    #[test]
    fn verify_ge_rejects_a_missing_version_file() {
        let d = tmpdir("nofile");
        assert!(matches!(verify_ge(&d), Err(VerifyError::Missing)));
    }

    #[test]
    fn verify_ge_tolerates_no_trailing_newline_and_extra_fields() {
        let d = tmpdir("loose");
        std::fs::write(d.join("version"), "1787951532 GE-Proton11-6 extra").unwrap();
        assert_eq!(verify_ge(&d).unwrap(), "GE-Proton11-6");
    }

    #[test]
    fn tags_order_numerically_not_lexically() {
        // "GE-Proton11-6" < "GE-Proton9-1" as strings, which would make 9 newer
        // than 11 and pick the wrong default runtime.
        assert_eq!(cmp_tags("GE-Proton11-6", "GE-Proton9-1"), Ordering::Greater);
        assert_eq!(cmp_tags("GE-Proton11-10", "GE-Proton11-9"), Ordering::Greater);
        assert_eq!(cmp_tags("GE-Proton11-6", "GE-Proton11-6"), Ordering::Equal);
    }

    #[test]
    fn installed_lists_only_verified_ge_runtimes_newest_first() {
        let base = tmpdir("installed");
        let root = crate::layout::Root::at(base.clone());
        std::fs::create_dir_all(root.runtimes()).unwrap();
        for (tag, body) in [
            ("GE-Proton11-6", "1 GE-Proton11-6\n"),
            ("GE-Proton9-1", "1 GE-Proton9-1\n"),
            ("junk-dir", "1 proton-9.0-4\n"),   // not GE -> excluded
            ("half-extracted", ""),              // no version file -> excluded
        ] {
            let d = root.runtime_dir(tag);
            std::fs::create_dir_all(&d).unwrap();
            if !body.is_empty() {
                std::fs::write(d.join("version"), body).unwrap();
            }
        }
        assert_eq!(
            installed(&root).unwrap(),
            vec!["GE-Proton11-6".to_string(), "GE-Proton9-1".to_string()]
        );
    }
}
