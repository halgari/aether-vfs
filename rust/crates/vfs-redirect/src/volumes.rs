//! Populate [`VolumeMap`] from the live OS at session start.
//!
//! `canon::canonicalise` needs a table from NT device names and volume-GUID
//! prefixes to the drive letter each is currently mounted as; that table
//! cannot be built without asking the OS (`QueryDosDeviceW`,
//! `GetVolumeNameForVolumeMountPointW`), and the OS answer can change while
//! the process runs (a drive can be mounted or unmounted), so this is a
//! snapshot taken once per session, not a pure function — the raw Win32
//! calls live in `vfs-win` (this crate is `#![forbid(unsafe_code)]`).
//!
//! Also home to 8.3 short-name (and junction/hardlink/`subst`) expansion,
//! for the same reason: the real path a spelling names is an OS fact, not
//! something derivable from the string alone.

use crate::canon::VolumeMap;

/// Build a [`VolumeMap`] covering every drive letter Windows currently
/// reports as mounted. Call once at session start; the map is a snapshot,
/// not a live view — a drive that is mounted or unmounted afterwards is not
/// reflected until this is called again.
///
/// Each drive's `\Device\...` name is registered as-is: NT device names are
/// already in their native NT-namespace spelling, so there is no rewrite
/// between what `QueryDosDeviceW` reports and what a real open presents to
/// `canon::resolve`.
///
/// Each drive's volume-GUID mount point is registered as `\??\Volume{guid}`,
/// **not** the `\\?\Volume{guid}` spelling `GetVolumeNameForVolumeMountPointW`
/// returns — see [`win32_guid_to_nt`] for why that distinction is load-bearing.
///
/// Relies on `vfs_win::drive_mappings` having already screened out any
/// `subst`/directory-alias target (`vfs_win::is_device_namespace_name`):
/// every `DriveMapping` this loop sees is guaranteed to carry a genuine
/// `\Device\...` name, never a drive alias, so this function itself never
/// needs to re-check the shape. See `vfs-win`'s `drive_mappings` docs for
/// why registering a `subst` alias here would be an active regression
/// (hijacking an already-correct in-root path), not just a missed vector.
pub fn resolve_volume_map() -> VolumeMap {
    let mut map = VolumeMap::empty();
    for m in vfs_win::drive_mappings() {
        map.insert(&m.device_name, m.drive);
        if let Some(win32_guid) = &m.volume_guid_win32 {
            if let Some(nt_guid) = win32_guid_to_nt(win32_guid) {
                map.insert(&nt_guid, m.drive);
            }
        }
    }
    map
}

/// Convert `GetVolumeNameForVolumeMountPointW`'s Win32 spelling of a
/// volume-GUID mount point (`\\?\Volume{guid}\`, trailing separator always
/// present) to the NT spelling `canon::resolve` actually matches against
/// (`\??\Volume{guid}`, no trailing separator).
///
/// This conversion is the whole point of this function existing separately:
/// a real open of `\\?\Volume{guid}\...` is rewritten by Windows to
/// `\??\Volume{guid}\...` before it ever reaches the NT layer the shim
/// hooks. `canon::canonicalise` consults `VolumeMap::resolve` on the raw
/// open path *before* it strips any NT/DOS prefix layer itself (see
/// `canon::canonicalise`'s body: `resolve_device_prefix` runs ahead of
/// `strip_all_nt_prefixes`), so the map is always queried with whatever
/// prefix the caller actually spelled — `\??\`, never `\\?\`, for a real
/// open. A map keyed with the Win32 spelling would never match a real path
/// and this whole vector would silently do nothing: not a bypass (canon
/// fails closed to "unmapped", same as any other unrecognised device), but
/// worse than useless, because an escape matrix built against the test's
/// convenience keying would report the vector as handled when it is not.
fn win32_guid_to_nt(win32_guid: &str) -> Option<String> {
    let rest = win32_guid.strip_prefix(r"\\?\")?;
    let rest = rest.trim_end_matches(['\\', '/']);
    if rest.is_empty() {
        None
    } else {
        Some(format!(r"\??\{rest}"))
    }
}

/// Expand any 8.3 short-name component, junction, hardlink, or
/// `subst`/mapped-drive spelling in `path` to its real, final path.
///
/// Prefers `GetFinalPathNameByHandleW` on an opened handle — the
/// authoritative answer, since it is what the OS itself resolves the open
/// to, and it collapses all four spellings above in one call — falling back
/// to `GetLongPathNameW` only when nothing could be opened at `path` (e.g.
/// it does not exist).
///
/// **Caution for callers feeding this into `canon`/`RootMap` (Task 3):** the
/// returned string is `GetFinalPathNameByHandleW`'s default `VOLUME_NAME_DOS`
/// form, which is `\\?\`-prefixed Win32 spelling — the exact sibling of the
/// volume-GUID trap fixed in `resolve_volume_map`/`win32_guid_to_nt`. A real
/// NT open never presents `\\?\`; it presents `\??\` (Windows rewrites one
/// to the other ahead of the NT layer). Treat this return value as needing
/// the same `\\?\` -> `\??\` normalisation before comparing it against, or
/// feeding it back into, anything that expects the NT spelling.
pub fn expand_short_name(path: &str) -> Option<String> {
    vfs_win::final_path_for_open(path).or_else(|| vfs_win::expand_long_path(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `resolve_volume_map` must map the current drive's `\Device\...` name,
    /// and the resulting map must resolve a device-prefixed open path to the
    /// same drive `canon::canonicalise` would give the plain drive-letter
    /// spelling.
    #[test]
    #[cfg(windows)]
    fn resolve_volume_map_maps_current_drive_device_name() {
        let drive = current_drive();
        let device_name = vfs_win::drive_mappings()
            .into_iter()
            .find(|m| m.drive.eq_ignore_ascii_case(&drive))
            .map(|m| m.device_name)
            .expect("current drive should be enumerated by drive_mappings");
        assert!(
            device_name.starts_with(r"\Device\"),
            "unexpected device name shape: {device_name}"
        );

        let map = resolve_volume_map();
        let raw = format!(r"{device_name}\some\path.txt");
        let got = crate::canon::canonicalise(&raw, &map).unwrap();
        assert!(
            got.to_ascii_lowercase().starts_with(&format!("{}:", drive.to_ascii_lowercase())),
            "device path did not resolve to drive {drive}: {got}"
        );
    }

    /// The handoff from Task 1's review: a volume-GUID path must be keyed
    /// the way it is *actually spelled* when a real open reaches
    /// `canon::resolve` — `\??\Volume{guid}`, not the `\\?\Volume{guid}`
    /// spelling `GetVolumeNameForVolumeMountPointW` returns. Exercise the
    /// real shape end-to-end: an NT-form volume-GUID path canonicalises to
    /// the same thing as the plain drive-letter path.
    #[test]
    #[cfg(windows)]
    fn volume_guid_key_uses_the_nt_prefix_a_real_open_presents() {
        let drive = current_drive();
        let win32_guid = vfs_win::drive_mappings()
            .into_iter()
            .find(|m| m.drive.eq_ignore_ascii_case(&drive))
            .and_then(|m| m.volume_guid_win32)
            .expect("current drive should have a volume-GUID mount point");
        assert!(
            win32_guid.starts_with(r"\\?\Volume{"),
            "unexpected volume GUID shape from the OS: {win32_guid}"
        );

        let map = resolve_volume_map();
        let nt_guid = win32_guid_to_nt(&win32_guid).unwrap();
        assert!(
            nt_guid.starts_with(r"\??\Volume{"),
            "converted key did not use the NT prefix: {nt_guid}"
        );

        // The real shape: what a shim hook actually receives after Windows
        // rewrites `\\?\` to `\??\` ahead of the NT layer.
        let raw = format!(r"{nt_guid}\some\path.txt");
        let plain = format!(r"{drive}:\some\path.txt");
        let via_guid = crate::canon::canonicalise(&raw, &map).unwrap();
        let via_drive = crate::canon::canonicalise(&plain, &VolumeMap::empty()).unwrap();
        assert_eq!(via_guid.to_ascii_lowercase(), via_drive.to_ascii_lowercase());

        // The convenience (Win32-spelled) key from Task 1's test fixture must
        // NOT match the real (NT-spelled) path a live open actually presents
        // — that mismatch is exactly the silent-failure mode this test
        // guards against.
        let mut wrong = VolumeMap::empty();
        wrong.insert(win32_guid.trim_end_matches('\\'), drive);
        let via_wrong_key = crate::canon::canonicalise(&raw, &wrong).unwrap();
        assert!(
            !via_wrong_key.to_ascii_lowercase().starts_with(&format!("{}:", drive.to_ascii_lowercase())),
            "a \\\\?\\-keyed map must not resolve a \\??\\-spelled real path"
        );
    }

    /// `expand_short_name` must round-trip a real 8.3 short name back to the
    /// long name it was generated from, going through a file this test
    /// actually creates (rather than assuming one exists on the machine).
    /// If 8.3 name generation is disabled on this volume, no distinct short
    /// name can be produced and the test is inconclusive by construction —
    /// that is Task 6's `unbuildable` case, not a reason to fail here.
    #[test]
    #[cfg(windows)]
    fn expand_short_name_round_trips_a_real_8dot3_name() {
        let dir = std::env::temp_dir().join(format!("vfs-redirect-8dot3-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let long_name = "ThisIsALongFileNameForRoundTripTesting.txt";
        let long_path = dir.join(long_name);
        std::fs::write(&long_path, b"x").unwrap();
        let long_str = long_path.to_string_lossy().into_owned();

        let short = vfs_win::short_path_name(&long_str).expect("GetShortPathNameW should succeed on an existing file");
        if short.eq_ignore_ascii_case(&long_str) {
            // 8.3 name generation is disabled on this volume: no distinct
            // short spelling exists to expand. Not this gate's failure.
            std::fs::remove_file(&long_path).ok();
            std::fs::remove_dir(&dir).ok();
            return;
        }

        let expanded = expand_short_name(&short).expect("expansion should succeed for a path that exists");
        assert!(
            expanded.to_ascii_lowercase().contains(&long_name.to_ascii_lowercase()),
            "expansion lost the long name: {expanded}"
        );
        assert!(
            !expanded.contains('~'),
            "expansion is still short-form: {expanded}"
        );

        std::fs::remove_file(&long_path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    /// A `subst`-shaped candidate (what `subst Z: C:\Games` makes
    /// `QueryDosDeviceW("Z:")` return: `\??\C:\Games`, a drive alias, not a
    /// device) must never survive into a `VolumeMap`, because its prefix
    /// matches a path that already canonicalised correctly — the
    /// `canon.rs` fixture path `\??\C:\Games\Skyrim\Data\a.esp` is exactly
    /// such a path. Constructed directly (no `subst` shelled out, whose
    /// effect would otherwise persist in the CI session) and gated on the
    /// same guard `vfs_win::drive_mappings` relies on
    /// (`vfs_win::is_device_namespace_name`), so this is a faithful replay
    /// of what `resolve_volume_map` actually assembles, not a hand-waved
    /// approximation. Flip-tested by hand: with the guard removed, this
    /// fails (the fixture path hijacks to `Z:`); with it restored, it
    /// passes.
    #[test]
    #[cfg(windows)]
    fn a_subst_shaped_candidate_is_screened_out_and_does_not_hijack_an_in_root_path() {
        let candidates: [(&str, char); 2] = [
            (r"\Device\HarddiskVolume3", 'C'),
            // What `subst Z: C:\Games` produces -- must never reach the map.
            (r"\??\C:\Games", 'Z'),
        ];
        let mut map = VolumeMap::empty();
        for (device_name, drive) in candidates {
            if vfs_win::is_device_namespace_name(device_name) {
                map.insert(device_name, drive);
            }
        }

        let raw = r"\??\C:\Games\Skyrim\Data\a.esp";
        let got = crate::canon::canonicalise(raw, &map).unwrap();
        assert!(
            got.to_ascii_lowercase().starts_with("c:"),
            "a subst-shaped candidate hijacked an in-root path: {got}"
        );
    }

    /// The current drive letter, from the working directory (always a real,
    /// currently-mounted drive while the test process is alive).
    #[cfg(windows)]
    fn current_drive() -> char {
        std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .chars()
            .next()
            .unwrap()
            .to_ascii_uppercase()
    }
}
