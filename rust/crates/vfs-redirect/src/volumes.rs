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
//!
//! Since the two-vector closeout (escape-matrix vectors 7 and 9), also home
//! to two more session-start alias sources that plug into the exact same
//! [`VolumeMap`] the device/volume-GUID table above uses:
//!
//! - the administrative UNC loopback share (`\\localhost\C$\...`, vector 9)
//!   — a fixed, cheap alias per mounted drive, no filesystem scan needed;
//! - a junction/reparse point that resolves into the managed root (vector 7)
//!   — needs a bounded filesystem scan; see [`junction_aliases`] for the
//!   scope this task chose and why.

use crate::canon::VolumeMap;

/// Build a [`VolumeMap`] covering every drive letter Windows currently
/// reports as mounted, the administrative UNC loopback share for each, and
/// any junction/reparse point resolved at session start to alias into
/// `root`. Call once at session start; the map is a snapshot, not a live
/// view — a drive mounted/unmounted, a share enabled/disabled, or a junction
/// created/retargeted afterwards is not reflected until this is called
/// again (the same staleness limit the device/volume-GUID table already
/// carries).
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
/// `root` may be NT (`\??\C:\Games\Skyrim`) or Win32 (`C:\Games\Skyrim`) —
/// the same either-form contract [`crate::RootMap::new`] accepts — and is
/// used only to scope the junction scan; it is not itself validated here
/// (an unresolvable or malformed root simply yields no junction aliases,
/// exactly as if none existed, which `RootMap::new`'s own error path already
/// handles for the rest of construction).
///
/// Relies on `vfs_win::drive_mappings` having already screened out any
/// `subst`/directory-alias target (`vfs_win::is_device_namespace_name`):
/// every `DriveMapping` this loop sees is guaranteed to carry a genuine
/// `\Device\...` name, never a drive alias, so this function itself never
/// needs to re-check the shape. See `vfs-win`'s `drive_mappings` docs for
/// why registering a `subst` alias here would be an active regression
/// (hijacking an already-correct in-root path), not just a missed vector.
pub fn resolve_volume_map(root: &str) -> VolumeMap {
    let mut map = VolumeMap::empty();
    for m in vfs_win::drive_mappings() {
        map.insert(&m.device_name, m.drive);
        if let Some(win32_guid) = &m.volume_guid_win32 {
            if let Some(nt_guid) = win32_guid_to_nt(win32_guid) {
                map.insert(&nt_guid, m.drive);
            }
        }
        map.insert_alias(&admin_share_nt_key(m.drive), &format!("{}:", m.drive));
    }
    for (location_nt, target_norm) in junction_aliases(root) {
        map.insert_alias(&location_nt, &target_norm);
    }
    map
}

/// The NT spelling a real open of `\\localhost\<drive>$\...` (Windows' own
/// built-in administrative loopback share for `drive`, present by default
/// unless explicitly disabled) presents to `NtCreateFile`.
///
/// Windows rewrites a Win32 UNC path (`\\server\share\...`) to
/// `\??\UNC\server\share\...` ahead of the NT layer — `\??\UNC` is a real
/// object-manager symlink, inside the same `\??\` (DosDevices) directory as
/// every other prefix this map matches against, to `\Device\Mup` (the
/// multiple-UNC-provider device). `\\localhost\C$\Games` therefore names
/// *exactly* the same object as `C:\Games`, the same shape of aliasing this
/// map already resolves for a bare device prefix or a `GLOBALROOT`-wrapped
/// one — see `canon::strip_globalroot_wrapper`'s doc comment for the
/// sibling case, and this project's own history of a volume-GUID key built
/// with the wrong (`\\?\`) prefix spelling matching nothing and failing
/// *closed*, which is the trap this key's `\??\` spelling avoids.
///
/// Scope, chosen deliberately: only the `localhost` hostname spelling is
/// registered, one alias per drive `vfs_win::drive_mappings` already
/// enumerates. The machine's own NetBIOS/DNS name (`\\MYPC\C$\...`) and
/// loopback address forms (`\\127.0.0.1\C$\...`, `\\[::1]\C$\...`) resolve
/// to the same object but are not aliased — closing every hostname spelling
/// that could ever resolve to "this machine" is unbounded, for marginal
/// benefit over the one spelling escape-matrix vector 9 actually exercises.
/// A documented gap, not an oversight — see `docs/escape-matrix.md`.
fn admin_share_nt_key(drive: char) -> String {
    format!(r"\??\UNC\localhost\{drive}$")
}

/// Strip a leading `\??\` / `\\?\` NT/DOS prefix, leaving a Win32-usable
/// path (drive intact) — the form `std::fs` and `vfs_win::final_path_for_open`
/// need, since `\??\...` is kernel-namespace notation `CreateFileW` itself
/// cannot parse.
fn strip_nt_prefix(p: &str) -> &str {
    p.strip_prefix(r"\??\").or_else(|| p.strip_prefix(r"\\?\")).unwrap_or(p)
}

/// Resolve `path` (a real, existing directory) to the NT-ready alias key a
/// hooked open of it would present: an ordinary absolute Win32 directory
/// path is always rewritten to its `\??\`-prefixed NT form ahead of the NT
/// layer, the same rule [`admin_share_nt_key`] and every other prefix in
/// this map already rely on.
fn nt_key_for_win32_path(win32_path: &str) -> String {
    format!(r"\??\{win32_path}")
}

/// Junction/reparse-point aliases resolved at session start: pairs of
/// `(location_nt, target_norm)` — `location_nt` is the NT spelling a real
/// open of the reparse point's own path presents, `target_norm` is the
/// `/`-joined, prefix-free canonical form of where it currently resolves —
/// ready to feed straight into `VolumeMap::insert_alias`.
///
/// **Scope, chosen deliberately.** Scanning an entire volume for reparse
/// points at session start is too slow (this must complete once per
/// session, not once per open, but it still has to complete once). Instead
/// this walks `root`'s own ancestor chain — `root`'s parent, that
/// directory's parent, and so on up to the drive root — doing exactly one
/// **non-recursive** directory listing per level. At each level, every
/// entry *other than the chain node itself* (see the safety note below for
/// why that exclusion is load-bearing, not cosmetic) is checked for being a
/// reparse point whose target lands inside `root`.
///
/// This is vector 7's actual shape: a junction *elsewhere* (e.g. a sibling
/// of the session's own temp-directory base — exactly how
/// `vfs-fixture-escape`'s vector 7 constructs its reparse point) whose
/// target is a subdirectory of the managed root. A candidate is registered
/// **only** if its resolved target's canonical form has `root`'s own
/// components as a prefix — the guard against the over-eager direction: a
/// sibling reparse point that leads anywhere else contributes nothing,
/// exactly the "must not pull anything in" requirement for an unrelated or
/// outward-pointing junction.
///
/// This bounds the scan to (ancestor depth) × (average directory size)
/// listings — small and roughly constant regardless of how large the
/// managed root's own content is.
///
/// **A narrower case was considered and rejected: an ancestor *itself*
/// being a reparse point** (e.g. `C:\Games` symlinked to a Steam library at
/// `D:\Library\Games`, with root spelled `C:\Games\Skyrim`). Aliasing the
/// ancestor to its target and letting the existing suffix-reattachment do
/// the rest looks tempting, but it is unsound: `RootMap`'s own root
/// components are always the *literal* spelling passed to `RootMap::new`,
/// never resolved through any junction, so aliasing one of root's own
/// ancestors would rewrite **every ordinary in-root open** (which
/// necessarily starts with that same ancestor's literal path) away from
/// matching root's own registered components — an active regression that
/// breaks legitimate traffic, not merely a missed vector, and a more
/// serious failure than either named vector left open. The chain node is
/// excluded from every level's candidate scan for exactly this reason: a
/// registered alias key must never be an ancestor of (or equal to) root's
/// own literal path, or it would intercept root's own already-correct
/// spelling too. Every entry this function actually returns is a genuine
/// *sibling* — by construction (a `read_dir` entry distinct from the
/// excluded chain node) disjoint from root's own ancestor chain — so this
/// invariant holds automatically rather than needing a runtime check.
///
/// **What this also does not do:** recurse into the managed root's own
/// subtree looking for a reparse point that points *out* of the root (e.g.
/// a Mod-Organizer-style staging junction, `root\Data\SomeMod` ->
/// `D:\Mods\SomeMod`). Under the same "target must resolve inside root"
/// discipline used above, a root-subtree junction only ever produces a
/// redundant no-op (its target is already inside root, so both spellings
/// already canonicalise correctly on their own) or would require admitting
/// a genuinely external, unrelated directory into the managed root —
/// exactly the over-eager failure class this project has already hit twice
/// (the `subst`-hijack and `GLOBALROOT`-wrapper regressions) and the one
/// this task's own brief calls out as the worse of the two directions. Left
/// as a documented non-goal, not a side effect of closing vector 7.
///
/// **Staleness, same limit as the rest of this map:** a junction created,
/// retargeted, or removed after session start is not reflected until the
/// session is rebuilt — identical to the `subst`/device-mapping limitation
/// [`resolve_volume_map`] already documents.
///
/// **Depth, bounded tightly and for a second, load-bearing reason beyond
/// cost.** Only [`MAX_ANCESTOR_LEVELS`] levels are climbed — deliberately
/// just enough to reach past this project's own two session-wrapper
/// directories (`vfs-directord::registry`'s `<TEMP>/vfs-daemon-<pid>-<seq>-<id>/root`:
/// one level for the per-session base directory, one more for the system
/// temp directory itself), not one level further into the broader user
/// profile tree. Climbing further was tried during verification and found
/// to cost more than time: a real Windows profile's own built-in
/// legacy-compatibility junctions (`AppData\Local\Application Data`,
/// `<profile>\Cookies`, `<profile>\SendTo`, and a dozen more) live exactly
/// one or two levels above a `%TEMP%`-rooted session, and reading even
/// just their on-disk metadata (never their targets — see
/// [`reparse_target_norm`]) for a dozen-plus extra entries added enough
/// latency to the session's *first* redirect decision to occasionally miss
/// the shim's own stats reporter's tick window in a fast, short-lived test
/// process (`hookstats::start_reporter`'s own doc comment: "nothing flushes
/// on exit... a process that exits before its first tick produces no
/// report file at all") — observed as a real, intermittent classification
/// miss during this task's own verification, not a hypothetical. A real
/// game session has no such tight timing window, but there is no reason to
/// pay the extra cost or the extra exposure to unrelated system junctions
/// when the fixed, two-level convention this project's own sessions always
/// use is already enough.
fn junction_aliases(root: &str) -> Vec<(String, String)> {
    const MAX_ANCESTOR_LEVELS: usize = 2;
    let root_win32 = strip_nt_prefix(root);
    let Ok(root_norm) = vfs_core::normalize_vpath(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut child = std::path::PathBuf::from(root_win32);
    for _ in 0..MAX_ANCESTOR_LEVELS {
        let Some(parent) = child.parent() else { break };
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                let candidate = entry.path();
                if candidate == child {
                    // Root's own ancestor chain — never aliased. See this
                    // function's doc comment: an alias key that is an
                    // ancestor of root's own literal path would intercept
                    // every ordinary in-root open too.
                    continue;
                }
                // Cheap pre-filter using data the directory enumeration
                // itself already returned (`FindNextFileW`'s own
                // `dwFileAttributes`/reparse tag) — no extra syscall, and
                // essential: an ordinary ancestor directory on a real,
                // long-used machine can hold thousands of entries (an
                // unremarkable `%TEMP%`), and a first version of this scan
                // called `std::fs::symlink_metadata` — a *separate*, real,
                // hooked query — on every single one of them regardless of
                // type, multiplying the shim's own `NtCreateFile` call
                // count by thousands for no reason. Found by reproduction:
                // an unrelated write-path e2e test's shim/director
                // open-count reconciliation started failing (opens the
                // director counted that the shim's own stats never showed)
                // until this landed, not a hypothetical concern.
                use std::os::windows::fs::FileTypeExt;
                let looks_like_a_reparse_dir =
                    entry.file_type().is_ok_and(|ft| ft.is_symlink_dir());
                if !looks_like_a_reparse_dir {
                    continue;
                }
                let Some(target_norm) = reparse_target_norm(&candidate) else { continue };
                if is_component_prefix(&root_norm, &target_norm) {
                    out.push((nt_key_for_win32_path(&candidate.to_string_lossy()), target_norm));
                }
            }
        }
        child = parent.to_path_buf();
    }
    out
}

/// If `path` is a directory reparse point, the `/`-joined, prefix-free
/// canonical form of where it currently points; `None` if it is not a
/// reparse point (or is one of a kind this project does not act on — a
/// cloud-file placeholder, deduplication, WOF, ...; see
/// `vfs_win::reparse_point_target`'s own doc comment), does not exist, or
/// cannot be read.
///
/// Callers are expected to have already cheaply pre-filtered with
/// `DirEntry::file_type()` (see `junction_aliases`) before reaching here —
/// this function's own work (`vfs_win::reparse_point_target`, one
/// `CreateFileW` + one `DeviceIoControl`) is real OS I/O, not free, even
/// though it is deliberately the non-target-following kind.
///
/// Resolves the target via `vfs_win::reparse_point_target`, **not**
/// `final_path_for_open`/`GetFinalPathNameByHandleW` — the latter opens
/// (and thus follows) the reparse point, which hung this exact scan on a
/// real profile's own pre-existing junctions during verification (see that
/// function's doc comment for the full account: an offline/disconnected
/// target device blocks on the OS's own timeout, tens of seconds, once per
/// such junction encountered while walking a real `Users\<name>` tree).
/// Reading the reparse point's own on-disk metadata never touches whatever
/// it points at.
fn reparse_target_norm(path: &std::path::Path) -> Option<String> {
    let real = vfs_win::reparse_point_target(&path.to_string_lossy())?;
    vfs_core::normalize_vpath(&real).ok()
}

/// Whether `candidate`'s normalized components start with all of `root`'s —
/// i.e. `candidate` names `root` itself or something under it. Case-folded,
/// component-wise, the same comparison [`crate::RootMap::match_canonical`]
/// uses for the same reason: a byte-for-byte or substring compare would
/// wrongly match `C:/Games2` against a root of `C:/Games`.
fn is_component_prefix(root: &str, candidate: &str) -> bool {
    let root_comps: Vec<&str> = if root.is_empty() { Vec::new() } else { root.split('/').collect() };
    let cand_comps: Vec<&str> =
        if candidate.is_empty() { Vec::new() } else { candidate.split('/').collect() };
    if cand_comps.len() < root_comps.len() {
        return false;
    }
    root_comps.iter().zip(cand_comps.iter()).all(|(r, c)| vfs_core::fold(r) == vfs_core::fold(c))
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

        let map = resolve_volume_map(&format!("{drive}:\\some-root"));
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

        let map = resolve_volume_map(&format!("{drive}:\\some-root"));
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

    /// A junction elsewhere whose target lands inside the constructed root
    /// must resolve into a `VolumeMap` alias that closes vector 7: opening a
    /// path through the junction canonicalises to the exact same form as
    /// the real root-rooted spelling. Mirrors `vfs-fixture-escape`'s own
    /// vector 7 construction — a junction placed in the system temp
    /// directory (elsewhere from `root`) pointing at a subdirectory of it —
    /// rather than a hand-waved approximation.
    #[test]
    #[cfg(windows)]
    fn junction_alias_closes_the_vector_7_shape() {
        let base =
            std::env::temp_dir().join(format!("vfs-redirect-junction-test-{}", std::process::id()));
        let root = base.join("root");
        let target_dir = root.join("Games").join("Skyrim").join("Data");
        std::fs::create_dir_all(&target_dir).unwrap();
        let link_dir = std::env::temp_dir()
            .join(format!("vfs-redirect-junction-link-{}", std::process::id()));
        let _ = std::fs::remove_dir(&link_dir);
        if !make_junction(&link_dir, &target_dir) {
            // No `mklink /J` support/privilege on this box — inconclusive,
            // not a failure of this gate (same posture as this file's
            // existing 8.3-name tests for a disabled feature).
            std::fs::remove_dir_all(&base).ok();
            return;
        }

        let root_str = root.to_string_lossy().into_owned();
        let map = resolve_volume_map(&root_str);
        let raw = format!(r"\??\{}\a.esp", link_dir.to_string_lossy());
        let plain = format!(r"\??\{}\a.esp", target_dir.to_string_lossy());
        let via_junction = crate::canon::canonicalise(&raw, &map).unwrap();
        let via_plain = crate::canon::canonicalise(&plain, &map).unwrap();
        assert_eq!(
            via_junction.to_ascii_lowercase(),
            via_plain.to_ascii_lowercase(),
            "a junction elsewhere pointing into root did not canonicalise to root's own form"
        );

        std::fs::remove_dir(&link_dir).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    /// Over-eager guard: a junction elsewhere whose target is a genuinely
    /// unrelated directory (outside the managed root entirely) must
    /// contribute nothing — the "must not pull anything in" direction this
    /// task's brief calls the more dangerous of the two failure modes.
    #[test]
    #[cfg(windows)]
    fn junction_pointing_outside_root_is_not_aliased() {
        let base = std::env::temp_dir()
            .join(format!("vfs-redirect-junction-outside-test-{}", std::process::id()));
        let root = base.join("root");
        let unrelated = base.join("unrelated");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&unrelated).unwrap();
        let link_dir = std::env::temp_dir()
            .join(format!("vfs-redirect-junction-outside-link-{}", std::process::id()));
        let _ = std::fs::remove_dir(&link_dir);
        if !make_junction(&link_dir, &unrelated) {
            std::fs::remove_dir_all(&base).ok();
            return;
        }

        let root_str = root.to_string_lossy().into_owned();
        let map = resolve_volume_map(&root_str);
        let raw = format!(r"\??\{}\a.esp", link_dir.to_string_lossy());
        let got = crate::canon::canonicalise(&raw, &map).unwrap();
        let root_norm = vfs_core::normalize_vpath(&root_str).unwrap();
        assert!(
            !is_component_prefix(&root_norm, &got),
            "a junction pointing to an unrelated directory was wrongly pulled into the root: {got}"
        );

        std::fs::remove_dir(&link_dir).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    /// Root's own ancestor chain must never be treated as an alias
    /// candidate, even when it is itself a reparse point — see
    /// `junction_aliases`'s doc comment for why aliasing it would be an
    /// active regression (rewriting every ordinary in-root open), not
    /// merely a missed vector. Exercises the exclusion directly: `root`
    /// itself is a junction, and no alias keyed to root's own path may be
    /// produced for it.
    #[test]
    #[cfg(windows)]
    fn root_itself_being_a_reparse_point_is_never_aliased() {
        let base = std::env::temp_dir()
            .join(format!("vfs-redirect-root-is-junction-test-{}", std::process::id()));
        let real_target = base.join("real-target");
        std::fs::create_dir_all(&real_target).unwrap();
        let root_link = base.join("root-link");
        let _ = std::fs::remove_dir(&root_link);
        if !make_junction(&root_link, &real_target) {
            std::fs::remove_dir_all(&base).ok();
            return;
        }

        let root_str = root_link.to_string_lossy().into_owned();
        let aliases = junction_aliases(&root_str);
        let root_key = nt_key_for_win32_path(&root_str);
        assert!(
            !aliases.iter().any(|(k, _)| k.eq_ignore_ascii_case(&root_key)),
            "root's own literal path was wrongly registered as an alias key: {aliases:?}"
        );

        std::fs::remove_dir(&root_link).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    /// `resolve_volume_map` registers the administrative UNC loopback share
    /// for the current drive: opening `\\localhost\<drive>$\...` in its real
    /// NT spelling (`\??\UNC\localhost\<drive>$\...`) must canonicalise
    /// identically to the plain drive-letter form.
    #[test]
    #[cfg(windows)]
    fn resolve_volume_map_registers_the_localhost_admin_share() {
        let drive = current_drive();
        let root = format!("{drive}:\\some-root");
        let map = resolve_volume_map(&root);
        let raw = format!(r"\??\UNC\localhost\{drive}$\some\path.txt");
        let plain = format!(r"{drive}:\some\path.txt");
        let via_share = crate::canon::canonicalise(&raw, &map).unwrap();
        let via_drive = crate::canon::canonicalise(&plain, &VolumeMap::empty()).unwrap();
        assert_eq!(via_share.to_ascii_lowercase(), via_drive.to_ascii_lowercase());
    }

    /// `mklink /J` needs no elevation, same convention `vfs-fixture-escape`
    /// and `skyrim-live`'s own `ensure_junction` already use. Returns
    /// whether it succeeded rather than panicking, so a box without
    /// junction support renders the calling test inconclusive rather than
    /// failing it.
    #[cfg(windows)]
    fn make_junction(link: &std::path::Path, target: &std::path::Path) -> bool {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J", &link.to_string_lossy(), &target.to_string_lossy()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
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
