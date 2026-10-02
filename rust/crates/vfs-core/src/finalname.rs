//! The name a handle is *finally* known by: where it lives, spelled the way
//! it is stored rather than the way it was asked for.
//!
//! `GetFinalPathNameByHandleW` — and so `std::filesystem::canonical` — answers
//! with a file's on-disk spelling whatever case the caller opened it in. Code
//! that checks containment relies on that: it takes `canonical(dir)` and
//! `canonical(dir/file)` and compares one as a prefix of the other, byte for
//! byte. For that to hold on a virtual tree, a directory handle and a handle
//! to a file under it must be named from one spelling, however either was
//! opened. That spelling is the one a listing of the parent reports, which is
//! what an enumeration shows and so what a caller can already observe.
//!
//! Pure: the listing is asked for through a closure.

use crate::fold;

/// The components of a DOS path, without its NT prefix: `\??\C:\A\b` and
/// `C:/A/b/` both give `["C:", "A", "b"]`.
fn components(path: &str) -> Vec<&str> {
    path.strip_prefix(r"\??\")
        .or_else(|| path.strip_prefix(r"\\?\"))
        .unwrap_or(path)
        .split(['\\', '/'])
        .filter(|c| !c.is_empty())
        .collect()
}

/// The caller's own spelling of the components `under` the root, when its
/// path ends in exactly those components (case apart). A device or
/// short-name spelling of the *root* still does; only the prefix differs.
/// `None` when it does not — a short name somewhere under the root, say.
pub fn opened_tail<'a>(opened: &'a str, under: &[String]) -> Option<Vec<&'a str>> {
    let asked = components(opened);
    let at = asked.len().checked_sub(under.len())?;
    let tail = &asked[at..];
    tail.iter()
        .zip(under)
        .all(|(a, u)| fold(a) == *u)
        .then(|| tail.to_vec())
}

/// The final DOS path (`C:\Games\Skyrim\Data\a.esp`, no NT prefix, no
/// trailing separator unless it is a bare drive root) of something opened as
/// `opened` that lies `under` a managed root.
///
/// - `opened` is the path as the caller spelled it, NT prefix or not.
/// - `under` is its folded components beneath the root — what the path
///   classifier already produced; empty for the root itself.
/// - `roots` are the spellings the root was declared with: the root's own
///   first, then any alias of it. The one the caller's prefix matches (case
///   apart) is used, in its declared spelling; the first if none does, which
///   is how a device-path or short-name spelling of the root is named.
/// - `stored` reports how the `i`-th component of `under` is spelled where
///   it is stored — the name a listing of its parent shows — or `None` if
///   that is not known.
///
/// A component with no stored spelling — a named stream, a file that is
/// gone, a director that cannot be asked — keeps the caller's spelling, so
/// the answer is never worse than the path that was opened.
pub fn final_dos_path(
    opened: &str,
    under: &[String],
    roots: &[&str],
    mut stored: impl FnMut(usize) -> Option<String>,
) -> String {
    let asked = components(opened);
    let tail = opened_tail(opened, under);
    let asked_prefix: Vec<String> = match tail {
        Some(_) => asked[..asked.len() - under.len()]
            .iter()
            .map(|c| fold(c))
            .collect(),
        None => Vec::new(),
    };
    let root = roots
        .iter()
        .find(|r| {
            let comps = components(r);
            comps.len() == asked_prefix.len()
                && comps.iter().zip(&asked_prefix).all(|(c, a)| fold(c) == *a)
        })
        .or(roots.first())
        .map(|r| components(r).join("\\"))
        .unwrap_or_default();

    let mut out = root;
    for (i, folded) in under.iter().enumerate() {
        let spelled = match (stored(i), &tail) {
            (Some(name), _) => name,
            (None, Some(tail)) => tail[i].to_string(),
            (None, None) => folded.clone(),
        };
        out.push('\\');
        out.push_str(&spelled);
    }
    // A bare drive is named with its root separator, as NTFS names it.
    if out.len() == 2 && out.ends_with(':') {
        out.push('\\');
    }
    out
}

/// [`final_dos_path`] with each component's spelling found in a listing of
/// its parent: `list` reports the names in the directory at a folded,
/// `/`-joined path under the root (`""` for the root), or `None` if it cannot
/// be listed.
///
/// One listing per component. This is how the shim first answered a name
/// query, and what the one-name lookup (`OP_STORED_NAMES`) replaced; it is
/// kept as the reference the lookup is tested and measured against.
pub fn final_dos_path_by_listing(
    opened: &str,
    under: &[String],
    roots: &[&str],
    mut list: impl FnMut(&str) -> Option<Vec<String>>,
) -> String {
    final_dos_path(opened, under, roots, |i| {
        let dir = under[..i].join("/");
        list(&dir).and_then(|names| names.into_iter().find(|n| fold(n) == under[i]))
    })
}

/// What a client remembers of how directories and files are spelled, so that
/// a name query does not cross to the director for components it was told
/// about a moment ago.
///
/// Keyed by root and folded path; each entry is the stored spelling of that
/// path's last component. An entry is used only while it is fresh (`ttl_ms`):
/// the client forgets what it changes itself ([`NameCache::forget`]), and the
/// time bound covers what somebody else changes.
pub struct NameCache {
    ttl_ms: u64,
    capacity: usize,
    names: std::collections::HashMap<(u32, String), (String, u64)>,
}

impl NameCache {
    /// A cache whose entries are good for `ttl_ms` and which holds at most
    /// `capacity` of them (it is emptied when full: simple, and a client
    /// that queries more distinct paths than that is not helped by a cache).
    pub fn new(ttl_ms: u64, capacity: usize) -> Self {
        NameCache {
            ttl_ms,
            capacity: capacity.max(1),
            names: std::collections::HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// The stored spellings of the leading components of `under` that are
    /// known and fresh at `now_ms`: as many as are, in order, stopping at the
    /// first that is not.
    pub fn leading(&self, root: u32, under: &[String], now_ms: u64) -> Vec<String> {
        let mut out = Vec::new();
        let mut path = String::new();
        for comp in under {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(comp);
            match self.names.get(&(root, path.clone())) {
                Some((name, at)) if now_ms.saturating_sub(*at) < self.ttl_ms => {
                    out.push(name.clone())
                }
                _ => break,
            }
        }
        out
    }

    /// Remember `names` as the spellings of `under[first..]`, learned at
    /// `now_ms`. Ignored unless there is exactly one name per component.
    pub fn remember(
        &mut self,
        root: u32,
        under: &[String],
        first: usize,
        names: &[String],
        now_ms: u64,
    ) {
        if first > under.len() || names.len() != under.len() - first {
            return;
        }
        if self.names.len() + names.len() > self.capacity {
            self.names.clear();
        }
        for (i, name) in names.iter().enumerate() {
            let path = under[..first + i + 1].join("/");
            self.names.insert((root, path), (name.clone(), now_ms));
        }
    }

    /// Forget how `folded` (a folded, `/`-joined path under `root`) and
    /// everything beneath it is spelled: it was created, renamed or removed.
    pub fn forget(&mut self, root: u32, folded: &str) {
        let folded = folded.trim_matches('/');
        if folded.is_empty() || folded == "." {
            self.names.retain(|(r, _), _| *r != root);
            return;
        }
        let below = format!("{folded}/");
        self.names
            .retain(|(r, path), _| *r != root || !(path == folded || path.starts_with(&below)));
    }
}

/// The volume-relative form of a DOS path — what `FileNameInformation` and
/// `FileNormalizedNameInformation` carry: `C:\A\b` gives `\A\b`, and a drive
/// root gives `\`.
pub fn volume_relative(dos: &str) -> &str {
    let b = dos.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        &dos[2..]
    } else {
        dos
    }
}

/// A stable 64-bit identity for whatever is at a final DOS path: the same for
/// every handle to one file, however each was opened, and different for
/// different files. What a file id is for (`std::filesystem::equivalent`
/// compares them); a per-handle number would make a file unequal to itself.
pub fn path_id(final_dos: &str) -> u64 {
    // FNV-1a over the folded path.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in fold(final_dos).bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Never zero, and never with the top bit set: callers hand it out as a
    // signed index.
    (h >> 1) | 1
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = r"C:\Haskill\tpf\game";

    /// A tree stored as `Data/Interface/CommunityShaders/Fonts/Jost/Jost-Regular.ttf`.
    fn list(dir: &str) -> Option<Vec<String>> {
        let names: &[&str] = match dir {
            "" => &["Data", "SkyrimSE.exe"],
            "data" => &["Interface", "Skyrim.esm"],
            "data/interface" => &["CommunityShaders"],
            "data/interface/communityshaders" => &["Fonts"],
            "data/interface/communityshaders/fonts" => &["Jost"],
            "data/interface/communityshaders/fonts/jost" => &["Jost-Regular.ttf"],
            _ => return None,
        };
        Some(names.iter().map(|s| s.to_string()).collect())
    }

    fn under(path: &str) -> Vec<String> {
        path.split('/')
            .filter(|c| !c.is_empty())
            .map(fold)
            .collect()
    }

    fn name(opened: &str, rel: &str) -> String {
        final_dos_path_by_listing(opened, &under(rel), &[ROOT], list)
    }

    const FONTS: &str = r"C:\Haskill\tpf\game\Data\Interface\CommunityShaders\Fonts";

    /// The case the whole module exists for: a directory and a file under it,
    /// each named from its own handle, and the directory's name is a prefix
    /// of the file's — compared as bytes, which is how callers compare them.
    #[test]
    fn a_directory_is_a_prefix_of_a_file_under_it() {
        let dir = name(
            r"\??\C:\Haskill\tpf\game\Data\Interface\CommunityShaders\Fonts",
            "Data/Interface/CommunityShaders/Fonts",
        );
        let file = name(
            r"\??\C:\Haskill\tpf\game\Data\Interface\CommunityShaders\Fonts\Jost\Jost-Regular.ttf",
            "Data/Interface/CommunityShaders/Fonts/Jost/Jost-Regular.ttf",
        );
        assert_eq!(dir, FONTS);
        assert_eq!(file, format!(r"{FONTS}\Jost\Jost-Regular.ttf"));
        assert!(file.starts_with(&format!(r"{dir}\")));
    }

    /// However the caller spelled it, the answer is the stored spelling — of
    /// the root's own path too, which is spelled as it was declared.
    #[test]
    fn the_spelling_is_the_stored_one_whatever_case_was_asked_for() {
        for opened in [
            r"\??\c:\haskill\TPF\GAME\data\INTERFACE\communityshaders\FONTS",
            r"C:/Haskill/tpf/game/Data/Interface/CommunityShaders/Fonts/",
            r"\\?\C:\HASKILL\TPF\GAME\DATA\INTERFACE\COMMUNITYSHADERS\FONTS",
        ] {
            assert_eq!(
                name(opened, "data/interface/communityshaders/fonts"),
                FONTS,
                "opened as {opened}"
            );
        }
    }

    #[test]
    fn the_root_itself_is_named_without_a_trailing_separator() {
        assert_eq!(name(r"\??\C:\Haskill\tpf\game", ""), ROOT);
        assert_eq!(name(r"\??\c:\HASKILL\tpf\game\", ""), ROOT);
        // And a file directly under it is that name plus one component.
        assert_eq!(
            name(r"\??\C:\haskill\tpf\game\skyrimse.EXE", "skyrimse.exe"),
            format!(r"{ROOT}\SkyrimSE.exe")
        );
    }

    /// A root that is a whole drive is `C:\`, and what is under it does not
    /// get a doubled separator.
    #[test]
    fn a_drive_root_keeps_its_separator_and_children_do_not_double_it() {
        let list = |dir: &str| (dir.is_empty()).then(|| vec!["Data".to_string()]);
        assert_eq!(
            final_dos_path_by_listing(r"\??\c:\", &[], &[r"C:\"], list),
            r"C:\"
        );
        assert_eq!(
            final_dos_path_by_listing(r"\??\c:\data", &under("data"), &[r"C:\"], list),
            r"C:\Data"
        );
    }

    /// What no listing has keeps the caller's spelling: a file created a
    /// moment ago, a named stream, anything under a directory that cannot be
    /// listed. Components above it are still stored-case.
    #[test]
    fn a_component_no_listing_has_keeps_the_callers_spelling() {
        assert_eq!(
            name(
                r"\??\C:\Haskill\tpf\game\DATA\New File.TXT",
                "data/new file.txt"
            ),
            format!(r"{ROOT}\Data\New File.TXT")
        );
        assert_eq!(
            name(
                r"\??\C:\Haskill\tpf\game\data\skyrim.esm:Zone.Identifier",
                "data/skyrim.esm:zone.identifier"
            ),
            format!(r"{ROOT}\Data\skyrim.esm:Zone.Identifier")
        );
        assert_eq!(
            name(
                r"\??\C:\Haskill\tpf\game\Data\Gone\Deeper\x.BIN",
                "data/gone/deeper/x.bin"
            ),
            format!(r"{ROOT}\Data\Gone\Deeper\x.BIN")
        );
    }

    /// An alias of the root (the staged launch directory) is named as the
    /// alias: the caller's prefix picks which declared spelling is used.
    #[test]
    fn a_path_under_an_alias_is_named_with_the_aliass_spelling() {
        let roots = [ROOT, r"C:\Stage\Vfs-Stage-1"];
        assert_eq!(
            final_dos_path_by_listing(
                r"\??\c:\stage\vfs-stage-1\DATA",
                &under("data"),
                &roots,
                list
            ),
            r"C:\Stage\Vfs-Stage-1\Data"
        );
        assert_eq!(
            final_dos_path_by_listing(
                r"\??\c:\haskill\tpf\game\DATA",
                &under("data"),
                &roots,
                list
            ),
            format!(r"{ROOT}\Data")
        );
    }

    /// A spelling of the root that names no declared path component by
    /// component — a device path, a volume GUID, an 8.3 name — is named as
    /// the root's own declared spelling. The part under the root is still
    /// read from the caller's path, which ends in it.
    #[test]
    fn an_unrecognised_spelling_of_the_root_is_named_as_the_declared_root() {
        assert_eq!(
            name(
                r"\Device\HarddiskVolume3\Haskill\tpf\game\DATA\NEW.txt",
                "data/new.txt"
            ),
            format!(r"{ROOT}\Data\NEW.txt")
        );
        // And when the caller's path does not even end in the components
        // under the root, those are the folded ones — never a guess.
        assert_eq!(
            name(r"\??\C:\HASKIL~1\tpf\game\DATA~1", "data/unlisted"),
            format!(r"{ROOT}\Data\unlisted")
        );
    }

    /// The spellings can come from anywhere: here, one answer per component,
    /// as the director's one-name lookup gives them.
    #[test]
    fn stored_spellings_are_taken_by_component_and_gaps_keep_the_callers() {
        let stored = ["Data", "Interface"];
        let named = final_dos_path(
            r"\??\c:\haskill\tpf\game\DATA\interface\New.SWF",
            &under("data/interface/new.swf"),
            &[ROOT],
            |i| stored.get(i).map(|s| s.to_string()),
        );
        assert_eq!(named, format!(r"{ROOT}\Data\Interface\New.SWF"));
    }

    #[test]
    fn the_callers_spelling_of_what_is_under_the_root_is_the_tail_of_its_path() {
        let u = under("data/skse/new log.txt");
        assert_eq!(
            opened_tail(r"\??\C:\Haskill\tpf\game\Data\SKSE\New Log.TXT", &u),
            Some(vec!["Data", "SKSE", "New Log.TXT"])
        );
        // The root spelled as a device path: the tail is still there.
        assert_eq!(
            opened_tail(r"\Device\HarddiskVolume3\x\DATA\skse\NEW LOG.txt", &u),
            Some(vec!["DATA", "skse", "NEW LOG.txt"])
        );
        // A short name under the root is not the caller spelling these
        // components at all.
        assert_eq!(
            opened_tail(r"C:\Haskill\tpf\game\DATA~1\SKSE\NEWLOG~1.TXT", &u),
            None
        );
        assert_eq!(opened_tail(r"C:\x", &u), None);
        assert_eq!(opened_tail(r"C:\Haskill\tpf\game", &[]), Some(vec![]));
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// The cache answers for the leading components it was told about, for
    /// as long as they are fresh, and for no others.
    #[test]
    fn the_name_cache_answers_leading_components_while_fresh() {
        let mut c = NameCache::new(1_000, 64);
        let fonts = under("data/interface/fonts");
        assert!(c.leading(0, &fonts, 0).is_empty());
        c.remember(0, &fonts, 0, &names(&["Data", "Interface", "Fonts"]), 10);
        assert_eq!(c.leading(0, &fonts, 500), ["Data", "Interface", "Fonts"]);
        // A file under it: its directories are known, it is not.
        let font = under("data/interface/fonts/a.ttf");
        assert_eq!(c.leading(0, &font, 500), ["Data", "Interface", "Fonts"]);
        c.remember(0, &font, 3, &names(&["A.ttf"]), 500);
        assert_eq!(c.leading(0, &font, 600).len(), 4);
        // Another root is another tree.
        assert!(c.leading(1, &fonts, 500).is_empty());
        // Stale: the directories were learned at 10 and are good until 1010.
        assert!(c.leading(0, &font, 1_010).is_empty());
        // A wrong number of names is not remembered at all.
        c.remember(1, &fonts, 0, &names(&["Data"]), 0);
        assert!(c.leading(1, &fonts, 0).is_empty());
    }

    /// What the client changes it forgets: the path itself and everything
    /// beneath it, and nothing beside it.
    #[test]
    fn the_name_cache_forgets_a_path_and_what_is_under_it() {
        let mut c = NameCache::new(u64::MAX, 64);
        let a = under("data/skse/plugins/a.log");
        let b = under("data/skse2/b.log");
        c.remember(0, &a, 0, &names(&["Data", "SKSE", "Plugins", "A.log"]), 0);
        c.remember(0, &b, 1, &names(&["SKSE2", "B.log"]), 0);
        c.remember(1, &a, 0, &names(&["d", "s", "p", "a"]), 0);

        c.forget(0, "data/skse");
        assert_eq!(
            c.leading(0, &a, 0),
            ["Data"],
            "the directory and below are gone"
        );
        assert_eq!(
            c.leading(0, &b, 0),
            ["Data", "SKSE2", "B.log"],
            "its neighbour is not"
        );
        assert_eq!(c.leading(1, &a, 0).len(), 4, "nor is another root");

        // The root itself: everything under that root.
        c.forget(0, ".");
        assert!(c.leading(0, &b, 0).is_empty());
        assert_eq!(c.leading(1, &a, 0).len(), 4);
    }

    #[test]
    fn a_full_name_cache_starts_again_rather_than_growing() {
        let mut c = NameCache::new(u64::MAX, 4);
        for i in 0..10 {
            c.remember(0, &under(&format!("d{i}/f")), 0, &names(&["D", "F"]), 0);
            assert!(c.len() <= 4);
        }
        assert_eq!(c.leading(0, &under("d9/f"), 0), ["D", "F"]);
    }

    #[test]
    fn the_volume_relative_name_drops_only_the_drive() {
        assert_eq!(
            volume_relative(FONTS),
            r"\Haskill\tpf\game\Data\Interface\CommunityShaders\Fonts"
        );
        assert_eq!(volume_relative(r"C:\"), r"\");
        assert_eq!(volume_relative(r"\already\relative"), r"\already\relative");
    }

    /// The two name classes and the object name must describe one path: the
    /// object name minus the volume-relative name is exactly the volume.
    #[test]
    fn the_volume_relative_name_is_a_suffix_of_the_full_one() {
        for path in [FONTS, ROOT, r"C:\"] {
            let rel = volume_relative(path);
            assert!(path.ends_with(rel));
            assert_eq!(&path[..path.len() - rel.len()], "C:");
        }
    }

    #[test]
    fn a_path_id_is_one_per_file_not_one_per_spelling() {
        let a = path_id(FONTS);
        assert_eq!(a, path_id(&FONTS.to_uppercase()));
        assert_ne!(a, path_id(&format!(r"{FONTS}\Jost")));
        assert_ne!(a, 0);
        assert!(a as i64 > 0, "usable as a signed file index");
    }
}
