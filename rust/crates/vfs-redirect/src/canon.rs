//! Canonicalise every NT spelling of a path to one form, so a device path,
//! long-UNC path, or volume-GUID path cannot slip past the shim's redirect
//! decision by looking unfamiliar.
//!
//! Pure: no filesystem access, no Windows API. Resolving `\Device\...` and
//! `\\?\Volume{...}` prefixes to a drive letter needs the OS (`QueryDosDevice`
//! and friends), so that lookup table is built elsewhere and handed in as a
//! [`VolumeMap`]. An unmapped device prefix is left untouched rather than
//! guessed at: see `an_unmapped_device_does_not_silently_become_a_drive`
//! below — treating an unrecognised device as some drive would make the VFS
//! start intercepting paths that were never under the managed root.
//!
//! A drive-rooted path also needs its own `..` handling: Windows clamps `..`
//! at the drive root (`C:\..` is `C:\`) rather than erroring or popping the
//! drive letter itself away, and letting the drive be popped away is a real
//! bypass — see `dotdot_traversal_through_the_drive_root_clamps_to_the_same_file`.

use vfs_core::{fold, normalize_vpath, PathError};

/// The object-manager tokens that name a *namespace* rather than a file, one
/// path component each. A leading run of these — in any order, any number of
/// times — names the same object as the path with the run removed, so
/// stripping is iterative (see [`strip_all_nt_prefixes`]) rather than a fixed
/// list of whole prefixes.
///
/// - `??` — the DosDevices object directory, as a real NT open spells it
///   (`\??\C:\...`).
/// - `GLOBAL??` — the *global* DosDevices directory that `\??` is a
///   per-session view of. `\GLOBAL??\C:\...` and `\??\C:\...` name the same
///   object; its absence here was half of the defect this list replaces.
/// - `Global` — the symlink, inside DosDevices, to `\GLOBAL??`, so
///   `\??\Global\C:\...` is another spelling of the same file again. Found
///   while enumerating siblings for this fix rather than reported: a direct
///   `NtCreateFile` probe of `\??\Global\C:\Windows\win.ini` and of
///   `\??\Global\GLOBALROOT\GLOBAL??\C:\Windows\win.ini` both returned
///   `STATUS_SUCCESS`.
/// - `?` — what Win32's verbatim marker `\\?\` reduces to once its separators
///   are set aside. Kept for the paths this crate is *handed* in that form
///   (`vfs_win::final_path_for_open` returns `VOLUME_NAME_DOS`,
///   `\\?\`-prefixed); a literal `\\?\...` presented to `NtCreateFile` is not
///   a working object name at all — measured: `STATUS_OBJECT_NAME_INVALID`,
///   because the object root has no empty-named entry — which is why it is
///   [`Namespace::Elsewhere`] rather than a DosDevices spelling, so a
///   `\??\`-keyed alias still does not match it. See
///   `unc_admin_share_alias_does_not_match_the_win32_spelling`.
/// - `GLOBALROOT` — the symlink, *inside* DosDevices, back to the object
///   manager's own root, so `\??\GLOBALROOT\Device\HarddiskVolume3\...` names
///   exactly what `\Device\HarddiskVolume3\...` does. It nests: a
///   `GLOBALROOT` can be followed by another object-directory token and
///   another `GLOBALROOT`, which is why nothing here assumes a bounded shape.
///
/// This list accepts a **superset** of what the object manager itself
/// resolves: `\??\GLOBALROOT\GLOBALROOT\...` and `\??\GLOBAL??\...` are both
/// stripped here but were measured to fail with `STATUS_OBJECT_PATH_NOT_FOUND`
/// (`GLOBALROOT` exists only *inside* a DosDevices directory, `GLOBAL??` only
/// at the object root). That asymmetry is deliberate and one-directional: a
/// spelling this list over-accepts gets classified as under-root and answered
/// by the director, which for an unopenable name means a not-found the OS was
/// going to give anyway. Under-accepting is the direction that reaches real
/// disk, and that is the failure this list exists to prevent.
const NAMESPACE_TOKENS: [(&str, Namespace); 5] = [
    ("?", Namespace::Elsewhere),
    ("??", Namespace::DosDevicesCanonical),
    ("GLOBAL??", Namespace::DosDevicesAliased),
    ("Global", Namespace::DosDevicesAliased),
    ("GLOBALROOT", Namespace::Elsewhere),
];

/// Where stripping a [`NAMESPACE_TOKENS`] entry leaves the path standing —
/// which decides whether the component now at the front can be looked up in
/// the [`VolumeMap`] under a *different* spelling than the one the caller
/// wrote. See [`resolve_aliases_and_strip_prefixes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Namespace {
    /// Inside the DosDevices directory, spelled the way every `VolumeMap` key
    /// is keyed (`\??\`). Nothing to re-spell: the string still carries that
    /// exact prefix, so the lookup already happened against it.
    DosDevicesCanonical,
    /// Inside the same directory under one of its *other* names (`\GLOBAL??`,
    /// `Global`). The entry that follows is the entry `\??\<entry>` names, so a
    /// failed bare lookup is worth retrying with the prefix re-spelled.
    DosDevicesAliased,
    /// Anywhere else — the object-manager root (behind `GLOBALROOT`), or
    /// Win32's verbatim marker (`?`), which is not an object directory at all.
    Elsewhere,
}

/// How many resolve-or-strip steps [`resolve_aliases_and_strip_prefixes`] will
/// take before giving up. Stripping a token strictly shortens the path, but
/// substituting an alias may lengthen it (a junction's target can be longer
/// than its location), so a pathological alias table could otherwise cycle
/// forever. Generous next to the deepest real shape (`\??\GLOBALROOT\GLOBAL??\`
/// is three tokens plus one substitution) and bounded either way: an
/// exhausted budget leaves the path partly resolved, which classifies as
/// outside — the same fail-closed direction as an unmapped device.
const MAX_PREFIX_STEPS: usize = 16;

/// A table from a raw NT-presented path prefix to the text that replaces it —
/// a drive letter (`C:`) for a device/volume-GUID prefix, or an arbitrary
/// absolute path for a junction/UNC-share alias that resolves into the
/// managed root. Built by resolving the OS's device namespace (`Task 2`) and,
/// since this task, the session's junction and administrative-UNC-share
/// aliases (`resolve_volume_map`); `empty()` is for tests that exercise none
/// of this.
///
/// One representation serves both needs: a device prefix is just an alias
/// whose replacement happens to be two bytes long (`X:`), so
/// [`resolve_alias`]'s `format!("{replacement}{rest}")` already does
/// the right thing for a multi-component replacement (a junction's real
/// target path) with no special-casing.
#[derive(Debug, Clone, Default)]
pub struct VolumeMap {
    entries: Vec<(String, String)>,
}

impl VolumeMap {
    /// A map with no device, volume-GUID, junction, or UNC-share aliases
    /// registered.
    pub fn empty() -> Self {
        VolumeMap { entries: Vec::new() }
    }

    /// Register an NT device prefix (e.g. `\Device\HarddiskVolume3`) or a
    /// volume-GUID prefix (e.g. `\\?\Volume{guid}`) as currently mounted on
    /// `drive`.
    pub fn insert(&mut self, prefix: &str, drive: char) {
        self.insert_alias(prefix, &format!("{drive}:"));
    }

    /// Register an arbitrary path alias: a raw path starting with `prefix`
    /// (matched the same way as [`Self::insert`] — case-insensitively, at a
    /// component boundary) canonicalises as if it had been spelled with
    /// `prefix` replaced by `replacement` instead. `replacement` may be a
    /// bare drive (`"C:"`, what [`Self::insert`] uses) or a full absolute
    /// path (what a junction alias needs) — either way it is fed back
    /// through the same downstream pipeline (`strip_all_nt_prefixes`,
    /// drive-relative rejection, `..` clamping, `normalize_vpath`) as if the
    /// caller had spelled `replacement` themselves, so a multi-component
    /// replacement works with no extra handling.
    pub fn insert_alias(&mut self, prefix: &str, replacement: &str) {
        self.entries.push((prefix.to_string(), replacement.to_string()));
    }

    /// If `path` starts with a registered prefix at a component boundary
    /// (the match ends the string or is followed by a separator, so
    /// `HarddiskVolume3` cannot match a path actually naming
    /// `HarddiskVolume30`), the replacement text and the byte length of the
    /// matched prefix. Longest match wins if more than one registered
    /// prefix matches.
    ///
    /// The match is case-insensitive: NT object-manager names are
    /// case-insensitive, so `\device\harddiskvolume3` is as valid a spelling
    /// as `\Device\HarddiskVolume3`, and Win32 directory paths are
    /// case-insensitive too. Both sides are folded for the comparison rather
    /// than lowercasing the stored key, so the returned replacement text
    /// keeps whatever case was registered.
    fn resolve(&self, path: &str) -> Option<(&str, usize)> {
        let mut best: Option<(&str, usize)> = None;
        for (prefix, replacement) in &self.entries {
            let Some(candidate) = path.get(..prefix.len()) else {
                continue;
            };
            if fold(candidate) != fold(prefix) {
                continue;
            }
            let rest = &path[prefix.len()..];
            if !rest.is_empty() && !rest.starts_with(['\\', '/']) {
                continue;
            }
            let is_longer = match best {
                Some((_, len)) => prefix.len() > len,
                None => true,
            };
            if is_longer {
                best = Some((replacement.as_str(), prefix.len()));
            }
        }
        best
    }
}

/// Strip one leading namespace component — any run of separators followed by
/// a [`NAMESPACE_TOKENS`] entry, ending at a component boundary — returning
/// what follows (its own leading separator intact, so the result is still an
/// absolute-looking path the next step can match against) and whether that
/// token left us inside the DosDevices object directory.
///
/// Requires at least one leading separator, so a *file* whose name happens to
/// match a token cannot be eaten: only a path rooted in the object-manager
/// namespace can begin `\GLOBALROOT\...`. `None` when the path does not begin
/// with such a component. Always shortens the path when it returns `Some`,
/// which is what makes the loop below terminate.
///
/// Matched with `eq_ignore_ascii_case` rather than `fold`: every token is
/// ASCII, so the two agree on which strings match, and this runs on every open
/// while `fold` allocates a `String` per comparison.
fn strip_one_namespace_token(s: &str) -> Option<(&str, Namespace)> {
    let body = s.trim_start_matches(['\\', '/']);
    if body.len() == s.len() {
        return None; // No leading separator: not a namespace-rooted path.
    }
    let end = body.find(['\\', '/']).unwrap_or(body.len());
    let (token, rest) = body.split_at(end);
    NAMESPACE_TOKENS
        .iter()
        .find(|(t, _)| t.eq_ignore_ascii_case(token))
        .map(|(_, namespace)| (rest, *namespace))
}

/// Strip every leading namespace component, however many are stacked and in
/// whatever order — see [`NAMESPACE_TOKENS`]. Idempotent by construction: it
/// loops until nothing more matches, so `\??\GLOBALROOT\GLOBAL??\C:\...`
/// comes out as `C:\...` exactly like the plain `\??\C:\...` does.
///
/// `normalize_vpath` strips only one layer (a single pass, `break`s after the
/// first match), which is fine for its own callers but not safe to rely on
/// here: a leftover layer would reach the final `normalize_vpath` call as an
/// ordinary-looking component (`?`, `??`, `GLOBAL??`, `GLOBALROOT`) and get
/// folded into the canonical path as if it were a real directory name — a
/// path that then matches no root and is answered by the real filesystem.
///
/// Leading separators are dropped along with the tokens, so a drive letter
/// ends up at the very start. That is load-bearing rather than cosmetic:
/// `canonicalise`'s drive-root `..` clamping only engages when the first
/// component *is* the drive, and a stray `\` in front of it (which is what
/// stripping `\GLOBAL??\` from `\GLOBAL??\C:\..\..\Windows` leaves behind)
/// would silently route the path to `normalize_vpath`'s own component-popping
/// instead, which pops the drive letter away too.
fn strip_all_nt_prefixes(s: &str) -> &str {
    let mut s = s;
    while let Some((rest, _)) = strip_one_namespace_token(s) {
        s = rest;
    }
    s.trim_start_matches(['\\', '/'])
}

/// [`strip_stream_suffix`] as a split: the path before any alternate-data-
/// stream suffix, and the suffix itself (**including** its leading colon),
/// or `None` when there is no stream.
///
/// Exposed because `canonicalise` deliberately *discards* the stream — its
/// job is to unify spellings of a file, and `f.esp:s` and `f.esp` name the
/// same file — but a caller building a vpath to send to the director must
/// not discard it. A named stream that does not exist has to come back
/// not-found; resolving it to the base path instead would answer a request
/// for `f.esp:s` with `f.esp`'s bytes. See `vfs-shim`'s
/// `FuseClient::vpath_under_root`, which re-attaches what this returns.
pub fn split_stream_suffix(raw: &str) -> (&str, Option<&str>) {
    let base = strip_stream_suffix(raw);
    if base.len() == raw.len() {
        (base, None)
    } else {
        (base, Some(&raw[base.len()..]))
    }
}

/// Split off a trailing alternate-data-stream suffix (`:stream` or
/// `:stream:$DATA`), returning the path before it. Runs before any
/// drive-letter or namespace inspection: a colon inside a stream name would
/// otherwise look like a drive-letter colon.
///
/// **Only the final component is searched.** A stream is a leaf — `dir:s\file`
/// names nothing, because there is nothing to traverse *through* a stream — so
/// a colon in any earlier component is not a stream separator, and cutting
/// there does not shorten a path, it destroys it. This is not hypothetical: it
/// is exactly how `\??\GLOBALROOT\GLOBAL??\C:\<root>\Data\a.esp` used to
/// canonicalise to `GLOBAL??/C`. The old rule measured a fixed set of whole
/// prefixes, found no drive letter behind the one it recognised (`GLOBALROOT`
/// is not `C:`), and so treated the drive colon several components later as
/// the stream separator — leaving a stub that matched no root, so the open
/// trampolined to the real file. Scoping the search to the final component
/// makes that class of mistake unreachable regardless of which prefix forms
/// are recognised.
///
/// Within that final component a *drive-letter* colon is still not a stream
/// separator; the first colon after it is. That case is real: `C:foo` (drive-
/// relative, one component, no separator at all) must survive intact for
/// [`is_drive_relative`] to refuse it, rather than being truncated to a
/// harmless-looking bare `C`.
fn strip_stream_suffix(raw: &str) -> &str {
    let comp_start = raw.rfind(['\\', '/']).map_or(0, |i| i + 1);
    let comp = &raw.as_bytes()[comp_start..];
    let has_drive_colon = comp.len() >= 2 && comp[0].is_ascii_alphabetic() && comp[1] == b':';
    let search_from = comp_start + if has_drive_colon { 2 } else { 0 };
    match raw[search_from..].find(':') {
        Some(rel) => &raw[..search_from + rel],
        None => raw,
    }
}

/// Substitute a registered device / volume-GUID / junction / UNC-share alias
/// if `path` starts with one, else `None`. An unregistered device prefix must
/// never be guessed into a drive, so "no match" means "leave it alone", not
/// "pick something".
fn resolve_alias(path: &str, volumes: &VolumeMap) -> Option<String> {
    volumes
        .resolve(path)
        .map(|(replacement, matched_len)| format!("{replacement}{}", &path[matched_len..]))
}

/// Peel the whole object-manager wrapper off `path`: at each step, substitute
/// a registered alias if one matches, otherwise strip one namespace token, and
/// repeat until neither applies. The result is a drive-rooted (`C:\...`) or
/// unrecognised-but-prefix-free path.
///
/// **Alias substitution is tried before stripping**, at every step, because
/// every [`VolumeMap`] key is registered *with* the prefix a real NT open
/// presents (`\??\Volume{guid}`, `\??\UNC\localhost\C$`, `\??\C:\...\junction`)
/// — stripping first would leave nothing for those keys to match.
///
/// **Both are looped**, because either can expose the other. A device name can
/// hide behind a namespace token (`\??\GLOBALROOT\Device\HarddiskVolume3\...`
/// is the same object as `\Device\HarddiskVolume3\...`: `GLOBALROOT` is a real
/// symlink, inside DosDevices, to the object manager's own root, and a
/// `VolumeMap` lookup only ever matches at the very start of the string, so it
/// never sees the device name with the token sitting in front of it). Equally,
/// a namespace token can sit behind an alias substitution. Neither nests to a
/// bounded depth, so neither is handled as a special case of the other.
///
/// **`\??`-re-spelling for a lookup, from an *aliased* DosDevices spelling
/// only.** A path that reached an entry of the DosDevices directory through
/// one of that directory's other names (`\GLOBAL??\...`, `\??\Global\...`,
/// either of them behind a `GLOBALROOT`) names the same entry as
/// `\??\<entry>`, so when the bare lookup fails the entry is looked up in that
/// one canonical spelling too — otherwise `\GLOBAL??\UNC\localhost\C$\...`
/// (measured: opens the real file, `STATUS_SUCCESS`) resolves no alias at all
/// while `\??\UNC\localhost\C$\...` resolves one. Two spellings do *not* get
/// this retry, each for its own reason:
///
/// - [`Namespace::DosDevicesCanonical`] (`??`) — the string it leaves behind is
///   the one the caller already spelled, so the retry would repeat the lookup
///   that just missed. This is the common case (every ordinary NT open is
///   `\??\C:\...`), so skipping it keeps the hot path at one lookup.
/// - [`Namespace::Elsewhere`] — for `GLOBALROOT` there is genuinely no
///   DosDevices entry at the front (the object root's own entries follow, e.g.
///   `\Device\...`), and for Win32's verbatim `?` marker the contract is that a
///   `\??\`-keyed alias must not match it at all, because a literal
///   `\\?\UNC\...` presented to `NtCreateFile` resolves to nothing — see
///   `unc_admin_share_alias_does_not_match_the_win32_spelling`.
fn resolve_aliases_and_strip_prefixes(path: &str, volumes: &VolumeMap) -> String {
    let mut cur = path.to_string();
    // Set when the token stripped last left the path standing on a DosDevices
    // entry spelled some way other than `\??\` — the one case worth a second,
    // re-spelled lookup.
    let mut respell_as_dosdevices = false;
    for _ in 0..MAX_PREFIX_STEPS {
        if let Some(next) = resolve_alias(&cur, volumes) {
            cur = next;
            respell_as_dosdevices = false;
            continue;
        }
        if respell_as_dosdevices {
            if let Some(next) = resolve_alias(&format!(r"\??{cur}"), volumes) {
                cur = next;
                respell_as_dosdevices = false;
                continue;
            }
        }
        match strip_one_namespace_token(&cur) {
            Some((rest, namespace)) => {
                cur = rest.to_string();
                respell_as_dosdevices = namespace == Namespace::DosDevicesAliased;
            }
            None => break,
        }
    }
    // Leading separators go with the tokens — see `strip_all_nt_prefixes`,
    // whose own trailing trim this mirrors (and which still runs after this,
    // for the prefix an alias's *replacement* text may itself carry).
    cur.trim_start_matches(['\\', '/']).to_string()
}

/// Strip trailing dots and spaces from each path component, the way Win32
/// silently discards them when resolving a name. `.` and `..` are left
/// alone — they are navigation, not a name Win32 would trim — so the
/// dot-dot handling below (and `normalize_vpath`'s) still sees them intact.
fn strip_trailing_punctuation(path: &str) -> String {
    path.split(['/', '\\'])
        .map(|comp| {
            if comp == "." || comp == ".." {
                comp
            } else {
                comp.trim_end_matches(['.', ' '])
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// `C:foo` — a drive letter directly followed by a non-separator character,
/// with no `\` or `/` in between — means "relative to the current directory
/// on drive C", state that belongs to the OS process and that this pure
/// function never has. Rather than guess (and risk guessing wrong, which is
/// exactly the failure mode this gate exists to close), canonicalise refuses
/// the spelling outright; resolving a CWD-relative open is the shim's job
/// elsewhere, not this gate's.
fn is_drive_relative(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && !matches!(b[2], b'\\' | b'/')
}

/// Whether a `/`-joined path's leading segment is a bare drive component
/// (`C:`, exactly two bytes). Produced only by a genuine absolute Win32 form
/// or by an alias's replacement text — never left over from a namespace
/// prefix, which [`resolve_aliases_and_strip_prefixes`] removes along with the
/// separators that followed it — so seeing this shape reliably means "there is
/// a drive root here to protect from `..`".
fn is_drive_component(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Resolve `.`/`..` in `remainder` — a `/`-joined path relative to a drive
/// or resolved-device root — clamping any `..` that would climb past that
/// root rather than erroring. This is what Windows actually does:
/// `C:\..` resolves to `C:\`, not to an error, and not to the drive letter
/// itself being popped away. `normalize_vpath` cannot provide this: it has
/// no notion of a drive boundary and pops every component — including a
/// leading drive letter — exactly like any other, which is how `..` was
/// able to climb past `C:` entirely and reappear as an ordinary,
/// non-drive-rooted path that no longer matched any root check.
fn clamp_dotdot(remainder: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for comp in remainder.split(['/', '\\']) {
        match comp {
            "" | "." => continue,
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.join("/")
}

/// Canonicalise one NT open path to a single form: strip any alternate-data-
/// stream suffix, then alternately resolve a device / volume-GUID / junction /
/// UNC-share alias via `volumes` and strip a leading object-manager namespace
/// token until neither applies (see [`resolve_aliases_and_strip_prefixes`] —
/// this is what makes every spelling of the DosDevices directory, stacked to
/// any depth, come out the same), refuse a
/// drive-relative (`C:foo`) spelling, strip trailing per-component
/// dots/spaces, clamp `..` at a drive boundary if one is present, then hand
/// the result to [`normalize_vpath`] for separator folding and any remaining
/// `.`/`..` resolution. Every spelling of the same file must come out
/// identical.
///
/// Case is preserved: folding case is the caller's job (`RootMap` already
/// folds for comparison), and `DiskProvider` needs the original case to open
/// the real file on disk.
pub fn canonicalise(raw: &str, volumes: &VolumeMap) -> Result<String, PathError> {
    let no_stream = strip_stream_suffix(raw);
    let resolved = resolve_aliases_and_strip_prefixes(no_stream, volumes);
    // An alias's replacement text is fed back through the same pipeline as if
    // the caller had spelled it (see `VolumeMap::insert_alias`), and may itself
    // carry a prefix; the loop above already strips what it produces, so this
    // is belt-and-braces on the last substitution rather than a second policy.
    let unprefixed = strip_all_nt_prefixes(&resolved);
    if is_drive_relative(unprefixed) {
        return Err(PathError::EscapesRoot);
    }
    let trimmed = strip_trailing_punctuation(unprefixed);
    match trimmed.split_once('/') {
        Some((maybe_drive, rest)) if is_drive_component(maybe_drive) => {
            let clamped = clamp_dotdot(rest);
            normalize_vpath(&format!("{maybe_drive}/{clamped}"))
        }
        _ => normalize_vpath(&trimmed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vols() -> VolumeMap {
        let mut v = VolumeMap::empty();
        v.insert(r"\Device\HarddiskVolume3", 'C');
        v.insert(r"\\?\Volume{12345678-1234-1234-1234-123456789abc}", 'C');
        v
    }

    /// Every spelling of the same file must produce one canonical form.
    #[test]
    fn all_spellings_agree() {
        let want = "c:/games/skyrim/data/a.esp";
        for raw in [
            r"C:\Games\Skyrim\Data\a.esp",
            r"\??\C:\Games\Skyrim\Data\a.esp",
            r"\\?\C:\Games\Skyrim\Data\a.esp",
            // Every object-manager spelling that reaches the drive letter
            // through a namespace token rather than a device name. These are
            // the spellings the stream-suffix rule used to truncate at the
            // drive colon (`\??\GLOBALROOT\GLOBAL??\C:\...` came out as
            // `GLOBAL??/C`), so they matched no root and the open reached real
            // disk. See `RootMap`'s own `NT_SPELLING_VECTORS` table for the
            // classification half.
            r"\GLOBAL??\C:\Games\Skyrim\Data\a.esp",
            r"\??\GLOBALROOT\GLOBAL??\C:\Games\Skyrim\Data\a.esp",
            r"\??\GLOBALROOT\??\C:\Games\Skyrim\Data\a.esp",
            r"\??\Global\C:\Games\Skyrim\Data\a.esp",
            r"\GLOBAL??\GLOBALROOT\GLOBAL??\C:\Games\Skyrim\Data\a.esp",
            r"\??\globalroot\global??\c:\games\skyrim\data\a.esp",
            r"\Device\HarddiskVolume3\Games\Skyrim\Data\a.esp",
            r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\Games\Skyrim\Data\a.esp",
            r"C:\Games\Skyrim\Data\.\a.esp",
            r"C:\Games\Skyrim\Other\..\Data\a.esp",
            r"C:/Games/Skyrim/Data/a.esp",
            r"C:\Games\Skyrim\Data\\a.esp",
            r"C:\GAMES\skyrim\DATA\A.ESP",
        ] {
            assert_eq!(
                canonicalise(raw, &vols()).unwrap().to_ascii_lowercase(),
                want,
                "spelling did not canonicalise: {raw}"
            );
        }
    }

    /// A stream suffix names the same file; the stream is not part of the path.
    #[test]
    fn strips_an_alternate_data_stream_suffix() {
        assert_eq!(
            canonicalise(r"C:\Games\Skyrim\Data\a.esp:evil", &vols()).unwrap().to_ascii_lowercase(),
            "c:/games/skyrim/data/a.esp"
        );
    }

    /// Win32 discards trailing dots and spaces; a path that differs only by
    /// them names the same file and must not escape by looking different.
    #[test]
    fn strips_trailing_dots_and_spaces_per_component() {
        for raw in [
            r"C:\Games\Skyrim\Data.\a.esp",
            r"C:\Games\Skyrim\Data \a.esp",
            r"C:\Games\Skyrim\Data\a.esp.",
        ] {
            assert_eq!(
                canonicalise(raw, &vols()).unwrap().to_ascii_lowercase(),
                "c:/games/skyrim/data/a.esp",
                "trailing punctuation was not stripped: {raw}"
            );
        }
    }

    /// A drive letter must not be confused with a volume it is not mapped to.
    #[test]
    fn an_unmapped_device_does_not_silently_become_a_drive() {
        let raw = r"\Device\HarddiskVolume9\Games\Skyrim\Data\a.esp";
        let got = canonicalise(raw, &vols()).unwrap();
        assert!(
            !got.to_ascii_lowercase().starts_with("c:"),
            "an unmapped device resolved to C: — {got}"
        );
    }

    /// With no drive or device prefix to clamp against, an excess `..` still
    /// has nowhere to go — this remains a hard error rather than a guess.
    /// (Replaces the old `escaping_dotdot_is_refused`, which asserted this
    /// for a *drive-rooted* path — see
    /// `single_dotdot_at_drive_root_keeps_the_drive` and
    /// `dotdot_traversal_through_the_drive_root_clamps_to_the_same_file`
    /// for why that assumption was wrong.)
    #[test]
    fn dotdot_escaping_with_no_drive_still_errors() {
        assert!(canonicalise(r"..\Windows\System32\evil.dll", &vols()).is_err());
    }

    /// NT object-manager device names are case-insensitive:
    /// `\device\harddiskvolume3` names the same volume as
    /// `\Device\HarddiskVolume3`.
    #[test]
    fn device_prefix_matches_case_insensitively() {
        let raw = r"\device\HARDDISKVOLUME3\Games\Skyrim\Data\a.esp";
        assert_eq!(
            canonicalise(raw, &vols()).unwrap().to_ascii_lowercase(),
            "c:/games/skyrim/data/a.esp"
        );
    }

    /// A doubled NT/DOS prefix must not corrupt or truncate the path — every
    /// stacked layer is stripped, not just the outermost one, and the
    /// alternate-data-stream check must not mistake an inner layer's drive
    /// colon for a stream separator.
    #[test]
    fn nested_prefixes_are_fully_stripped() {
        let raw = r"\??\\\?\C:\Games\Skyrim\Data\a.esp";
        assert_eq!(
            canonicalise(raw, &vols()).unwrap().to_ascii_lowercase(),
            "c:/games/skyrim/data/a.esp"
        );
    }

    /// `C:foo` (no separator after the drive colon) means "relative to
    /// drive C's current directory" — state this pure function never has.
    /// Rather than guess, canonicalise refuses the spelling outright; the
    /// shim's existing CWD-relative-open handling is the right place to
    /// resolve it, not this gate.
    #[test]
    fn drive_relative_form_is_rejected() {
        assert!(canonicalise(r"C:Games\Skyrim\Data\a.esp", &vols()).is_err());
    }

    /// `..` cannot pop past the drive letter itself. Windows clamps at the
    /// drive root rather than erroring — and critically, rather than
    /// silently discarding the drive and reappearing as an ordinary
    /// non-drive-rooted path, which is exactly how this traversal could
    /// slip past a root check undetected while the OS still opened the
    /// in-root file.
    #[test]
    fn dotdot_traversal_through_the_drive_root_clamps_to_the_same_file() {
        let plain = canonicalise(r"C:\Games\Skyrim\Data\a.esp", &vols()).unwrap();
        let traversal =
            canonicalise(r"C:\Games\Skyrim\..\..\..\Games\Skyrim\Data\a.esp", &vols()).unwrap();
        assert_eq!(traversal, plain);
    }

    /// A single `..` at the drive root clamps to the drive root; the drive
    /// letter must survive, not be popped away.
    #[test]
    fn single_dotdot_at_drive_root_keeps_the_drive() {
        let got = canonicalise(r"C:\..\Windows\System32\evil.dll", &vols()).unwrap();
        assert!(
            got.to_ascii_lowercase().starts_with("c:"),
            "the drive did not survive a `..` at the root — {got}"
        );
    }

    /// `\\?\GLOBALROOT\Device\...` names exactly the same object as
    /// `\Device\...` — the `GLOBALROOT` object-manager symlink must not
    /// hide a registered device prefix from `VolumeMap::resolve`. Found via
    /// Task 6's session-based escape matrix: this exact wrapped spelling
    /// (`vfs-fixture-escape`'s vector 3) reached the real file on disk via
    /// plain OS passthrough while going completely unclassified by
    /// `RootMap::under_root` — reachable, but invisible to every counter.
    #[test]
    fn globalroot_wrapped_device_prefix_resolves_like_the_bare_form() {
        let bare = canonicalise(r"\Device\HarddiskVolume3\Games\Skyrim\Data\a.esp", &vols());
        let wrapped = canonicalise(
            r"\??\GLOBALROOT\Device\HarddiskVolume3\Games\Skyrim\Data\a.esp",
            &vols(),
        );
        assert_eq!(bare.unwrap(), wrapped.unwrap());
    }

    /// The `\\?\` spelling of the same wrapper (what a Win32
    /// `\\?\GLOBALROOT\...` open actually presents to `NtCreateFile` after
    /// Windows' own `\\?\` -> `\??\` rewrite is a *different* case from this
    /// pure function's perspective — both NT/DOS prefix spellings must work).
    #[test]
    fn globalroot_wrapper_matches_either_nt_dos_prefix_spelling() {
        let a = canonicalise(r"\??\GLOBALROOT\Device\HarddiskVolume3\a.esp", &vols()).unwrap();
        let b = canonicalise(r"\\?\GLOBALROOT\Device\HarddiskVolume3\a.esp", &vols()).unwrap();
        assert_eq!(a, b);
    }

    /// Case-insensitively, like every other NT object-manager name.
    #[test]
    fn globalroot_wrapper_matches_case_insensitively() {
        let got = canonicalise(
            r"\??\globalroot\device\harddiskvolume3\Games\Skyrim\Data\a.esp",
            &vols(),
        );
        assert_eq!(got.unwrap().to_ascii_lowercase(), "c:/games/skyrim/data/a.esp");
    }

    /// An unmapped device behind a `GLOBALROOT` wrapper must still not be
    /// guessed into a drive — the same fail-closed rule as the bare form.
    #[test]
    fn globalroot_wrapped_unmapped_device_does_not_silently_become_a_drive() {
        let got = canonicalise(
            r"\??\GLOBALROOT\Device\HarddiskVolume9\Games\Skyrim\Data\a.esp",
            &vols(),
        )
        .unwrap();
        assert!(
            !got.to_ascii_lowercase().starts_with("c:"),
            "an unmapped device behind a GLOBALROOT wrapper resolved to C: — {got}"
        );
    }

    /// An ordinary drive-letter path (no namespace token anywhere) must
    /// resolve exactly as before — the resolve-or-strip loop must never
    /// disturb the common case, only the spellings that carry a wrapper.
    #[test]
    fn globalroot_fallback_does_not_disturb_an_ordinary_path() {
        let got = canonicalise(r"C:\Games\Skyrim\Data\a.esp", &vols()).unwrap();
        assert_eq!(got.to_ascii_lowercase(), "c:/games/skyrim/data/a.esp");
    }

    /// An entry of the DosDevices directory reached through a *different*
    /// spelling of that directory is still the same entry, so an alias keyed
    /// with the one prefix a real open presents (`\??\...`, what
    /// `resolve_volume_map` registers) must still be found — otherwise
    /// `\GLOBAL??\UNC\localhost\C$\...`, measured to open the real file with
    /// `STATUS_SUCCESS`, resolves no alias at all and lands outside every
    /// root. Both halves matter: the volume-GUID key and the admin-share
    /// alias are the two `\??\`-keyed entries production registers.
    #[test]
    fn a_dosdevices_entry_resolves_through_any_spelling_of_that_directory() {
        let mut v = VolumeMap::empty();
        v.insert(r"\??\Volume{12345678-1234-1234-1234-123456789abc}", 'C');
        v.insert_alias(r"\??\UNC\localhost\C$", "C:");
        for raw in [
            r"\??\Volume{12345678-1234-1234-1234-123456789abc}\Games\Skyrim\Data\a.esp",
            r"\GLOBAL??\Volume{12345678-1234-1234-1234-123456789abc}\Games\Skyrim\Data\a.esp",
            r"\??\GLOBALROOT\GLOBAL??\Volume{12345678-1234-1234-1234-123456789abc}\Games\Skyrim\Data\a.esp",
            r"\??\UNC\localhost\C$\Games\Skyrim\Data\a.esp",
            r"\GLOBAL??\UNC\localhost\C$\Games\Skyrim\Data\a.esp",
            r"\??\Global\UNC\localhost\C$\Games\Skyrim\Data\a.esp",
        ] {
            assert_eq!(
                canonicalise(raw, &v).unwrap().to_ascii_lowercase(),
                "c:/games/skyrim/data/a.esp",
                "a `\\??\\`-keyed alias was not found behind another spelling of DosDevices: {raw}"
            );
        }
    }

    /// Vector 7 (junction): `insert_alias` with a *multi-component* absolute
    /// path as the replacement — a junction's own location aliased to its
    /// real target — must resolve exactly like the target had been spelled
    /// directly, remainder and all. This is the mechanism
    /// `resolve_volume_map`'s ancestor scan uses to close the junction
    /// vector: `VolumeMap` itself does not care whether a replacement is two
    /// bytes (a drive letter) or a whole path.
    #[test]
    fn insert_alias_resolves_a_multi_component_junction_target() {
        let mut v = vols();
        v.insert_alias(
            r"\??\C:\Users\me\AppData\Local\Temp\vfs-escape-junction-1234",
            "C:/Games/Skyrim/Data",
        );
        let raw = r"\??\C:\Users\me\AppData\Local\Temp\vfs-escape-junction-1234\a.esp";
        assert_eq!(
            canonicalise(raw, &v).unwrap().to_ascii_lowercase(),
            "c:/games/skyrim/data/a.esp"
        );
    }

    /// Vector 9 (UNC admin share): the NT spelling of `\\localhost\C$\...`
    /// (`\??\UNC\localhost\C$\...` — the `\??\UNC` object-manager symlink to
    /// `\Device\Mup`, exactly the sibling trap already found once for
    /// volume-GUID keys) must resolve identically to the plain drive-letter
    /// form once the alias is registered.
    #[test]
    fn insert_alias_resolves_the_unc_admin_share_nt_spelling() {
        let mut v = vols();
        v.insert_alias(r"\??\UNC\localhost\C$", "C:");
        let raw = r"\??\UNC\localhost\C$\Games\Skyrim\Data\a.esp";
        assert_eq!(
            canonicalise(raw, &v).unwrap().to_ascii_lowercase(),
            "c:/games/skyrim/data/a.esp"
        );
    }

    /// The sibling of the `GLOBALROOT` trap this gate already found once: a
    /// real NT open of `\\localhost\C$\...` presents `\??\UNC\...`, never
    /// the Win32 `\\?\UNC\...` spelling — a map keyed with the latter would
    /// match nothing and fail *closed* (not a bypass, but silently useless,
    /// exactly the failure this test guards against reintroducing).
    #[test]
    fn unc_admin_share_alias_does_not_match_the_win32_spelling() {
        let mut v = vols();
        v.insert_alias(r"\??\UNC\localhost\C$", "C:");
        let raw = r"\\?\UNC\localhost\C$\Games\Skyrim\Data\a.esp";
        let got = canonicalise(raw, &v).unwrap();
        assert!(
            !got.to_ascii_lowercase().starts_with("c:"),
            "a \\??\\-keyed UNC alias must not match the \\\\?\\-spelled convenience form: {got}"
        );
    }

    /// Over-eager guard: an alias registered for one junction/UNC location
    /// must never engage for an unrelated path that merely shares a prefix
    /// substring but not a full component match (`...-1234` vs `...-12345`).
    #[test]
    fn alias_does_not_match_a_similarly_prefixed_unrelated_path() {
        let mut v = vols();
        v.insert_alias(r"\??\C:\Temp\vfs-escape-junction-1234", "C:/Games/Skyrim/Data");
        let raw = r"\??\C:\Temp\vfs-escape-junction-12345\a.esp";
        let got = canonicalise(raw, &v).unwrap();
        assert!(
            !got.to_ascii_lowercase().starts_with("c:/games"),
            "an alias hijacked an unrelated, similarly-named sibling path: {got}"
        );
    }
}
