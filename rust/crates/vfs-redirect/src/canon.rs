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

/// NT/DOS long-path prefixes `normalize_vpath` also recognises, in both
/// slash forms.
const NT_PREFIXES: [&str; 4] = [r"\??\", r"\\?\", "/??/", "//?/"];

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
/// `resolve_device_prefix`'s `format!("{replacement}{rest}")` already does
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

/// Strip every leading recognised NT/DOS prefix, looping in case more than
/// one layer is stacked. `normalize_vpath` strips only one layer (a single
/// pass, `break`s after the first match), which is fine for its own callers
/// but not safe for us to rely on here: after we peel a prefix ourselves for
/// other reasons (see `nt_prefix_len` and `resolve_device_prefix`), a second
/// leftover layer would otherwise reach the final `normalize_vpath` call as
/// an ordinary-looking component (`?` or `??`) and get folded into the
/// canonical path as if it were a real directory name.
fn strip_all_nt_prefixes(s: &str) -> &str {
    let mut s = s;
    while let Some(p) = NT_PREFIXES.iter().find(|p| s.starts_with(**p)) {
        s = &s[p.len()..];
    }
    s
}

/// The byte length of every leading recognised NT/DOS prefix, stacked or
/// not. Used only to find where a drive letter or device name starts for
/// the alternate-data-stream check below; a naive single-layer measurement
/// would stop after the outer prefix and mistake a nested prefix's own
/// drive colon for a stream separator, truncating (not just failing to
/// unify) the rest of the path.
fn nt_prefix_len(s: &str) -> usize {
    s.len() - strip_all_nt_prefixes(s).len()
}

/// Split off a trailing alternate-data-stream suffix (`:stream` or
/// `:stream:$DATA`), returning the path before it. Must run before any
/// drive-letter or device-prefix inspection: a colon inside a stream name
/// would otherwise look like (or be confused with) a drive-letter colon.
///
/// A drive-letter colon (`C:`) — right after any NT/DOS prefix that is
/// present — is not itself a stream separator; the first colon *after* it
/// is.
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

fn strip_stream_suffix(raw: &str) -> &str {
    let prefix_len = nt_prefix_len(raw);
    let rest = &raw[prefix_len..];
    let rest_bytes = rest.as_bytes();
    let has_drive_colon =
        rest_bytes.len() >= 2 && rest_bytes[0].is_ascii_alphabetic() && rest_bytes[1] == b':';
    let search_from = prefix_len + if has_drive_colon { 2 } else { 0 };
    match raw[search_from..].find(':') {
        Some(rel) => &raw[..search_from + rel],
        None => raw,
    }
}

/// Resolve a leading `\Device\...` or `\\?\Volume{...}` prefix to its drive
/// letter via `volumes`. Paths with no such prefix, or with a prefix not
/// present in `volumes`, are returned unchanged — an unmapped device must
/// never be guessed into a drive.
fn resolve_device_prefix(path: &str, volumes: &VolumeMap) -> String {
    match volumes.resolve(path) {
        Some((replacement, matched_len)) => format!("{replacement}{}", &path[matched_len..]),
        None => path.to_string(),
    }
}

/// `GLOBALROOT` is a real, well-known symlink *inside* the `\??\`
/// (DosDevices) object directory that points straight at the object
/// manager's own root (`\`) — so `\??\GLOBALROOT\Device\HarddiskVolume3\...`
/// (what Windows presents to `NtCreateFile` for a
/// `\\?\GLOBALROOT\Device\HarddiskVolume3\...` Win32 open, the trick that
/// reaches a device name without going through an ordinary `\??\`
/// (DosDevices) symlink at all — see `vfs-fixture-escape`'s vector 3) names
/// *exactly* the same object as the bare `\Device\HarddiskVolume3\...` form.
///
/// Left unstripped, `resolve_device_prefix`'s `VolumeMap` lookup — which
/// only ever matches a registered prefix *at the very start* of the string —
/// would never see the `\Device\...`/`\\?\Volume{...}` text at all, because
/// the literal `GLOBALROOT` token sits in front of it. The whole spelling
/// would then canonicalise as an unrecognised, non-drive-rooted path:
/// `RootMap::under_root` would answer "outside" for a path the OS itself
/// resolves identically to the un-wrapped form — invisible to every
/// counter, exactly the failure mode this gate exists to close. Found by
/// reproduction (Task 6's session-based matrix), not asserted in advance:
/// the OS-level open still succeeds either way (`tramp` doesn't care what
/// `RootMap` thinks), so nothing about *reachability* ever signalled this
/// gap — only classification did.
///
/// Returns the text after `GLOBALROOT` (starting with the following
/// separator, so the caller can feed it straight back into
/// `resolve_device_prefix`), or `None` if `path` has no such wrapper.
fn strip_globalroot_wrapper(path: &str) -> Option<&str> {
    const TOKEN: &str = "GLOBALROOT";
    for prefix in NT_PREFIXES {
        let Some(rest) = path.strip_prefix(prefix) else { continue };
        let Some(candidate) = rest.get(..TOKEN.len()) else { continue };
        if fold(candidate) != fold(TOKEN) {
            continue;
        }
        let after = &rest[TOKEN.len()..];
        if after.is_empty() || after.starts_with(['\\', '/']) {
            return Some(after);
        }
    }
    None
}

/// [`resolve_device_prefix`], additionally trying a `GLOBALROOT`-wrapped
/// spelling if the bare form does not match. The bare form is tried first
/// and wins outright when it matches, so an ordinary (unwrapped) device or
/// volume-GUID path resolves exactly as it always did — this is a pure
/// fallback for the wrapped shape, never a second, competing way to resolve
/// the common case.
fn resolve_device_prefix_with_globalroot(path: &str, volumes: &VolumeMap) -> String {
    let bare = resolve_device_prefix(path, volumes);
    if bare != path {
        return bare;
    }
    match strip_globalroot_wrapper(path) {
        Some(unwrapped) => resolve_device_prefix(unwrapped, volumes),
        None => bare,
    }
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
/// or by `resolve_device_prefix`'s replacement — never left over from a
/// generic NT/DOS prefix — so seeing this shape reliably means "there is a
/// drive root here to protect from `..`".
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
/// stream suffix, resolve a device or volume-GUID prefix to its drive letter
/// via `volumes`, strip every stacked NT/DOS prefix layer, refuse a
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
    let resolved = resolve_device_prefix_with_globalroot(no_stream, volumes);
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

    /// An ordinary drive-letter path (no `GLOBALROOT` anywhere) must resolve
    /// exactly as before — the fallback must never engage for the common
    /// case, only for a spelling the bare match already failed on.
    #[test]
    fn globalroot_fallback_does_not_disturb_an_ordinary_path() {
        let got = canonicalise(r"C:\Games\Skyrim\Data\a.esp", &vols()).unwrap();
        assert_eq!(got.to_ascii_lowercase(), "c:/games/skyrim/data/a.esp");
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
