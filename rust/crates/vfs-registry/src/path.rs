//! Canonical NT registry key paths.

pub const CURRENT_USER: &str = "<CurrentUser>";
const CLASSES: &str = "_Classes";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// Not under `\Registry`.
    NotRegistry,
    /// Empty, `.` or `..` component where one is not allowed.
    BadComponent,
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::NotRegistry => f.write_str("path is not under \\Registry"),
            PathError::BadComponent => f.write_str("empty, \".\" or \"..\" path component"),
        }
    }
}

impl std::error::Error for PathError {}

/// Canonical form of an NT key path: `\Registry\...`, single separators, no trailing `\`.
///
/// `Registry` and the hive name (`Machine`, `User`) get their conventional spelling; everything
/// below keeps the caller's. Under `\Registry\User`, `user_sid` and `user_sid + "_Classes"`
/// (matched case-insensitively) become [`CURRENT_USER`] and `CURRENT_USER + "_Classes"`, so a
/// layer written for one account reads back for another. Comparison stays case-insensitive
/// through [`fold`].
pub fn canonical(nt_path: &str, user_sid: Option<&str>) -> Result<String, PathError> {
    let mut parts = nt_path.split('\\').filter(|p| !p.is_empty());
    match parts.next() {
        Some(p) if nt_path.starts_with('\\') && fold(p) == "registry" => {}
        _ => return Err(PathError::NotRegistry),
    }
    let mut out = String::from("\\Registry");
    let mut user_hive = false;
    for (i, part) in parts.enumerate() {
        out.push('\\');
        match i {
            0 => {
                let f = fold(part);
                user_hive = f == "user";
                out.push_str(match f.as_str() {
                    "machine" => "Machine",
                    "user" => "User",
                    _ => part,
                });
            }
            1 if user_hive => out.push_str(&sid_to_symbol(part, user_sid)),
            _ => out.push_str(part),
        }
    }
    Ok(out)
}

fn sid_to_symbol(part: &str, user_sid: Option<&str>) -> String {
    if let Some(sid) = user_sid {
        let f = fold(part);
        let fs = fold(sid);
        if f == fs {
            return CURRENT_USER.to_string();
        }
        if f == format!("{fs}{}", fold(CLASSES)) {
            return format!("{CURRENT_USER}{CLASSES}");
        }
    }
    part.to_string()
}

/// The reverse of [`canonical`], for names handed back to the process
/// (`NtQueryKey` `KeyNameInformation`, `NtQueryObject`): `\REGISTRY\MACHINE` and
/// `\REGISTRY\USER`, as Windows spells them, and the user's SID for the symbol. With no SID
/// the symbol is left as is.
pub fn to_nt(canonical: &str, user_sid: Option<&str>) -> String {
    let parts: Vec<&str> = canonical.split('\\').collect();
    // `\Registry\User\<sid>`: split gives ["", "Registry", "User", <sid>, ...].
    let user_hive = parts.get(2).is_some_and(|p| fold(p) == "user");
    let mut out = String::with_capacity(canonical.len() + user_sid.map_or(0, str::len));
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push('\\');
        }
        match (i, user_sid) {
            // The spelling Windows itself reports in key names.
            (1, _) if fold(part) == "registry" => out.push_str("REGISTRY"),
            (2, _) if fold(part) == "machine" => out.push_str("MACHINE"),
            (2, _) if fold(part) == "user" => out.push_str("USER"),
            (3, Some(sid)) if user_hive && *part == CURRENT_USER => out.push_str(sid),
            (3, Some(sid)) if user_hive && *part == format!("{CURRENT_USER}{CLASSES}") => {
                out.push_str(sid);
                out.push_str(CLASSES);
            }
            _ => out.push_str(part),
        }
    }
    out
}

/// Case fold used for every lookup (`vfs_core::fold`).
pub fn fold(s: &str) -> String {
    vfs_core::fold(s)
}

pub fn parent(canonical: &str) -> Option<&str> {
    match canonical.rfind('\\') {
        Some(0) | None => None,
        Some(i) => Some(&canonical[..i]),
    }
}

pub fn leaf(canonical: &str) -> &str {
    canonical.rsplit('\\').next().unwrap_or(canonical)
}

/// `base\rel`; rejects `.`/`..` and empty components in `rel` (including a leading or trailing
/// separator, or an empty `rel`).
pub fn join(base: &str, rel: &str) -> Result<String, PathError> {
    if rel
        .split('\\')
        .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(PathError::BadComponent);
    }
    Ok(format!("{base}\\{rel}"))
}

/// Under `\Registry\Machine` or `\Registry\User`: the parts of the registry the overlay serves.
pub fn is_virtualised(canonical: &str) -> bool {
    let f = fold(canonical);
    ["\\registry\\machine", "\\registry\\user"]
        .iter()
        .any(|h| f == *h || f.starts_with(&format!("{h}\\")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "S-1-5-21-111-222-333-1001";
    const B: &str = "S-1-5-21-999-888-777-1002";

    #[test]
    fn canonicalises_case_separators_and_trailing() {
        assert_eq!(
            canonical(r"\REGISTRY\MACHINE\Software\\X\", None).unwrap(),
            r"\Registry\Machine\Software\X"
        );
        assert_eq!(
            canonical(r"\registry\user\Foo", None).unwrap(),
            r"\Registry\User\Foo"
        );
        assert_eq!(canonical(r"\Registry", None).unwrap(), r"\Registry");
    }

    #[test]
    fn rejects_non_registry_paths() {
        assert_eq!(canonical(r"\Device\X", None), Err(PathError::NotRegistry));
        assert_eq!(canonical("", None), Err(PathError::NotRegistry));
        assert_eq!(canonical(r"Software\X", None), Err(PathError::NotRegistry));
    }

    #[test]
    fn sid_replaced_both_ways_including_classes() {
        let nt = format!(r"\REGISTRY\USER\{A}\Software\Bethesda");
        let c = canonical(&nt, Some(A)).unwrap();
        assert_eq!(c, r"\Registry\User\<CurrentUser>\Software\Bethesda");
        assert_eq!(to_nt(&c, Some(A)), nt);

        let nt = format!(r"\REGISTRY\USER\{A}_Classes\CLSID");
        let c = canonical(&nt, Some(A)).unwrap();
        assert_eq!(c, r"\Registry\User\<CurrentUser>_Classes\CLSID");
        assert_eq!(to_nt(&c, Some(A)), nt);
    }

    #[test]
    fn sid_match_is_case_insensitive_and_other_sids_stay() {
        let nt = format!(r"\Registry\User\{}", A.to_lowercase());
        assert_eq!(
            canonical(&nt, Some(A)).unwrap(),
            r"\Registry\User\<CurrentUser>"
        );
        let other = format!(r"\Registry\User\{B}\X");
        assert_eq!(canonical(&other, Some(A)).unwrap(), other);
        assert_eq!(canonical(&nt, None).unwrap(), nt);
        // Only the hive-level component is a SID.
        let deep = format!(r"\Registry\Machine\{A}");
        assert_eq!(canonical(&deep, Some(A)).unwrap(), deep);
    }

    #[test]
    fn layer_written_with_sid_a_reads_with_sid_b() {
        let written = canonical(&format!(r"\Registry\User\{A}\Software\Mod"), Some(A)).unwrap();
        let from_b = canonical(&format!(r"\Registry\User\{B}\Software\Mod"), Some(B)).unwrap();
        assert_eq!(written, from_b);
        assert_eq!(
            to_nt(&written, Some(B)),
            format!(r"\REGISTRY\USER\{B}\Software\Mod")
        );
        // No SID known: left symbolic.
        assert_eq!(
            to_nt(&written, None),
            r"\REGISTRY\USER\<CurrentUser>\Software\Mod"
        );
        // HKLM, and the case of everything below the hive kept.
        assert_eq!(
            to_nt(r"\Registry\Machine\Software\Bethesda", Some(A)),
            r"\REGISTRY\MACHINE\Software\Bethesda"
        );
    }

    #[test]
    fn parent_leaf_join() {
        assert_eq!(parent(r"\Registry\A\B"), Some(r"\Registry\A"));
        assert_eq!(parent(r"\Registry"), None);
        assert_eq!(leaf(r"\Registry\A\B"), "B");
        assert_eq!(join(r"\Registry\A", "B").unwrap(), r"\Registry\A\B");
        assert_eq!(join(r"\Registry\A", r"B\C").unwrap(), r"\Registry\A\B\C");
    }

    #[test]
    fn join_rejects_dotdot_and_empty_parts() {
        assert_eq!(join(r"\Registry\A", ".."), Err(PathError::BadComponent));
        assert_eq!(
            join(r"\Registry\A", r"B\..\C"),
            Err(PathError::BadComponent)
        );
        assert_eq!(join(r"\Registry\A", ""), Err(PathError::BadComponent));
        assert_eq!(join(r"\Registry\A", r"B\\C"), Err(PathError::BadComponent));
        assert_eq!(join(r"\Registry\A", r"\B"), Err(PathError::BadComponent));
        assert_eq!(join(r"\Registry\A", r"B\"), Err(PathError::BadComponent));
    }

    #[test]
    fn virtualised_is_machine_and_user_only() {
        assert!(is_virtualised(r"\Registry\Machine"));
        assert!(is_virtualised(r"\Registry\Machine\Software"));
        assert!(is_virtualised(r"\Registry\User\<CurrentUser>\X"));
        assert!(!is_virtualised(r"\Registry\MachineX"));
        assert!(!is_virtualised(r"\Registry"));
        assert!(!is_virtualised(r"\Registry\WC"));
    }

    #[test]
    fn fold_is_case_insensitive() {
        assert_eq!(fold("AbC"), fold("aBc"));
    }
}
