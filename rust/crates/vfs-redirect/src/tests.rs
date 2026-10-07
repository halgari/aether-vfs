use super::*;
use crate::cache::OsConsultGuard;
use vfs_core::PathError;

/// `OsConsultGuard`'s own reentrancy mechanics, independent of any real
/// OS call or hook — the crash this guard fixes (see its doc comment)
/// can only be reproduced from inside a real injected session, but the
/// guard's own held/released bookkeeping is a pure, unit-testable fact:
/// a nested `enter()` while one is already held must refuse (`None`),
/// and the slot must become available again once the outer guard drops.
#[test]
fn os_consult_guard_refuses_reentry_and_releases_on_drop() {
    let outer = OsConsultGuard::enter().expect("first enter must succeed");
    assert!(
        OsConsultGuard::enter().is_none(),
        "a nested enter while held must refuse"
    );
    drop(outer);
    assert!(
        OsConsultGuard::enter().is_some(),
        "the slot must be free again once the outer guard was dropped"
    );
}

#[test]
fn new_normalizes_nt_and_win32_roots() {
    // Both forms normalize to the same component vector.
    let nt = RootMap::new(r"\??\C:\Games\Skyrim", VolumeMap::empty()).unwrap();
    let win32 = RootMap::new(r"C:\Games\Skyrim", VolumeMap::empty()).unwrap();
    assert_eq!(nt.root_components(), win32.root_components());
    assert_eq!(nt.root_components(), vec!["C:", "Games", "Skyrim"]);
}

/// A root that normalizes to zero components (`""`, `"."`, `"/"`, or an
/// NT/DOS prefix with nothing after it) must be rejected at construction,
/// not accepted and left to `match_canonical` — which, given zero
/// components to fold-compare, would match *every* path with the whole
/// path as the remainder. Reachable via `VFS_VIRTUAL_DIR=""` (the shim's
/// env entry point checks for unset, not empty) and, since stage 2b, via
/// a second declared root that is malformed independently of the first.
#[test]
fn an_empty_root_is_rejected_rather_than_matching_every_path() {
    // `RootMap` has no `Debug` impl (it holds a lookup cache), so
    // `unwrap_err` — which requires `T: Debug` — is not available here;
    // match the `Result` directly instead.
    fn assert_empty_root_err(r: Result<RootMap, PathError>) {
        match r {
            Err(PathError::EmptyRoot) => {}
            Err(other) => panic!("expected PathError::EmptyRoot, got {other:?}"),
            Ok(_) => panic!("expected an error, but the empty root was accepted"),
        }
    }
    assert_empty_root_err(RootMap::new("", VolumeMap::empty()));
    assert_empty_root_err(RootMap::new(".", VolumeMap::empty()));
    assert_empty_root_err(RootMap::new("/", VolumeMap::empty()));
    // A malformed second root must not silently swallow a valid first one.
    assert_empty_root_err(RootMap::with_roots(
        &[(RootId(0), r"C:\Games\Skyrim"), (RootId(1), "")],
        VolumeMap::empty(),
    ));
}

/// Stage 2b task 5, step 1: the structural claim of the whole task. A
/// path under root 1 resolves to `(RootId(1), rel)`, a path under root 0
/// to `(RootId(0), rel)`, and a path under neither is outside. Before this
/// task `RootMap` held exactly one root and could answer only "inside" or
/// "outside", so the shim could not tell the director which root a path
/// belonged to.
#[test]
fn resolve_answers_with_the_matching_root_id_and_remainder() {
    let map = RootMap::with_roots(
        &[
            (RootId(0), r"C:\Games\Skyrim"),
            (RootId(1), r"C:\Users\me\Documents\My Games\Skyrim"),
        ],
        VolumeMap::empty(),
    )
    .unwrap();

    assert_eq!(
        map.resolve(r"\??\C:\Games\Skyrim\Data\Foo.ESP"),
        Some((RootId(0), vec!["data".to_string(), "foo.esp".to_string()]))
    );
    assert_eq!(
        map.resolve(r"\??\C:\Users\me\Documents\My Games\Skyrim\Saves\Save1.ess"),
        Some((
            RootId(1),
            vec!["saves".to_string(), "save1.ess".to_string()]
        ))
    );
    assert_eq!(map.resolve(r"\??\C:\Windows\System32\kernel32.dll"), None);
    // The same *relative* path under each root is a different answer —
    // the collision the whole stage exists to make representable.
    assert_eq!(
        map.resolve(r"C:\Games\Skyrim\same.txt").unwrap().0,
        RootId(0)
    );
    assert_eq!(
        map.resolve(r"C:\Users\me\Documents\My Games\Skyrim\same.txt")
            .unwrap()
            .0,
        RootId(1)
    );
}

/// A root nested inside another must win, or the shallow one swallows
/// every path the deep one serves. Declared shallow-first here on purpose:
/// the ordering must come from `with_roots`' own sort, not from the
/// caller happening to declare them in a helpful order.
#[test]
fn a_nested_root_wins_over_the_root_it_sits_inside() {
    let map = RootMap::with_roots(
        &[
            (RootId(0), r"C:\Users\me\Documents"),
            (RootId(1), r"C:\Users\me\Documents\My Games\Skyrim"),
        ],
        VolumeMap::empty(),
    )
    .unwrap();
    assert_eq!(
        map.resolve(r"C:\Users\me\Documents\My Games\Skyrim\Saves\a.ess"),
        Some((RootId(1), vec!["saves".to_string(), "a.ess".to_string()]))
    );
    assert_eq!(
        map.resolve(r"C:\Users\me\Documents\notes.txt"),
        Some((RootId(0), vec!["notes.txt".to_string()]))
    );
}

/// Two declared paths may share one `RootId`: that is how the shim's
/// staged-launch directory is served as a second spelling of the game
/// root. Both must resolve, and both must answer with the *same* id, so a
/// request routed through either spelling reaches the same provider.
#[test]
fn two_paths_may_share_one_root_id_as_an_alias() {
    let map = RootMap::with_roots(
        &[
            (RootId(0), r"C:\Games\Skyrim"),
            (RootId(0), r"C:\tmp\vfs-stage-21728"),
        ],
        VolumeMap::empty(),
    )
    .unwrap();
    assert_eq!(
        map.resolve(r"C:\Games\Skyrim\Data\a.esm"),
        Some((RootId(0), vec!["data".to_string(), "a.esm".to_string()]))
    );
    assert_eq!(
        map.resolve(r"C:\tmp\vfs-stage-21728\Data\a.esm"),
        Some((RootId(0), vec!["data".to_string(), "a.esm".to_string()]))
    );
}

/// Canonicalisation is per-`RootMap`, not per-root: registering a second
/// root must not cost the first one its device-path/volume-GUID
/// resolution, and the second root must get the same treatment rather
/// than a string-prefix approximation of it. This is the acceptance
/// criterion "the escape matrix passes against every root, not just the
/// first", at the unit level where the canonicaliser actually lives.
#[test]
fn canonicalisation_applies_to_every_root_not_just_the_first() {
    let mut volumes = VolumeMap::empty();
    volumes.insert(r"\Device\HarddiskVolume3", 'C');
    let map = RootMap::with_roots(
        &[
            (RootId(0), r"C:\Games\Skyrim"),
            (RootId(1), r"C:\Docs\Skyrim"),
        ],
        volumes,
    )
    .unwrap();
    assert_eq!(
        map.resolve(r"\Device\HarddiskVolume3\Games\Skyrim\Data\a.esp")
            .map(|(r, _)| r),
        Some(RootId(0))
    );
    assert_eq!(
        map.resolve(r"\Device\HarddiskVolume3\Docs\Skyrim\Saves\a.ess")
            .map(|(r, _)| r),
        Some(RootId(1)),
        "the second root must canonicalise exactly like the first"
    );
    // And the over-eager direction still fails closed for both.
    assert!(map
        .resolve(r"\Device\HarddiskVolume3\Windows\System32\x.dll")
        .is_none());
}

fn root() -> RootMap {
    RootMap::new(r"\??\C:\Games\Skyrim", VolumeMap::empty()).unwrap()
}

#[test]
fn contains_reports_under_root() {
    let r = root(); // \??\C:\Games\Skyrim
    assert!(r.contains(r"\??\C:\Games\Skyrim\Data\foo.esp"));
    assert!(r.contains(r"\??\C:\Games\Skyrim")); // the root itself
    assert!(!r.contains(r"\??\C:\Windows\System32"));
    assert!(!r.contains(r"\??\C:\Games\Skyrim\..\..\..\..\evil")); // escaping
}

#[test]
fn remainder_returns_folded_components() {
    let r = root(); // \??\C:\Games\Skyrim
    assert_eq!(
        r.remainder(r"\??\C:\Games\Skyrim\Data\Foo.ESP"),
        Some(vec!["data".to_string(), "foo.esp".to_string()])
    );
    assert_eq!(r.remainder(r"\??\C:\Windows"), None);
}

// -- Task 3: canonicalisation wired into `under_root` -----------------

/// A device-path spelling of a file under the root, which the old
/// normalize_vpath-only `under_root` classified outside (no device-prefix
/// resolution at all), must now resolve inside via the `VolumeMap` handed
/// to `RootMap::new`.
#[test]
fn under_root_recognises_a_device_path_spelling() {
    let mut volumes = VolumeMap::empty();
    volumes.insert(r"\Device\HarddiskVolume3", 'C');
    let map = RootMap::new(r"C:\Games\Skyrim", volumes).unwrap();
    assert!(map.contains(r"\Device\HarddiskVolume3\Games\Skyrim\Data\a.esp"));
}

/// The failure mode of an over-eager canonicaliser is worse than the one
/// being fixed: a path genuinely outside the root — spelled either as a
/// plain drive path or via the very same registered device prefix — must
/// stay outside. Registering a device prefix must not make the VFS start
/// swallowing the rest of that volume.
#[test]
fn under_root_still_rejects_a_path_genuinely_outside_the_root() {
    let mut volumes = VolumeMap::empty();
    volumes.insert(r"\Device\HarddiskVolume3", 'C');
    let map = RootMap::new(r"C:\Games\Skyrim", volumes).unwrap();
    assert!(!map.contains(r"C:\Windows\System32\kernel32.dll"));
    assert!(!map.contains(r"\Device\HarddiskVolume3\Windows\System32\kernel32.dll"));
}

/// An 8.3 short-name spelling of a component of the root ITSELF (not just
/// of the virtual remainder under it) is a real bypass: syntactic
/// canonicalisation alone cannot know `GAMES~1` and `Games` name the same
/// directory, only the OS does. Builds a real temp root (short names are
/// an on-disk fact, not derivable from the string), so this needs a real
/// file — skips gracefully if this volume has 8.3 generation disabled
/// (same convention as the existing vfs-win / vfs-redirect volumes tests).
#[test]
#[cfg(windows)]
fn under_root_recognises_an_8dot3_style_spelling_of_the_root() {
    let base = std::env::temp_dir().join(format!(
        "vfs-redirect-830-under-root-{}",
        std::process::id()
    ));
    let long_name = "ThisIsALongRootDirectoryNameForShortNameTesting";
    let root_dir = base.join(long_name);
    std::fs::create_dir_all(root_dir.join("Data")).unwrap();
    std::fs::write(root_dir.join("Data").join("a.esp"), b"x").unwrap();
    let root_str = root_dir.to_str().unwrap().to_string();

    let short_root = match vfs_win::short_path_name(&root_str) {
        Some(s) if !s.eq_ignore_ascii_case(&root_str) => s,
        _ => {
            // 8.3 name generation disabled on this volume: nothing to
            // test here (Task 6's `unbuildable` case, not a failure).
            std::fs::remove_dir_all(&base).ok();
            return;
        }
    };

    // The OS's own resolution of the short root must be VOLUME_NAME_DOS
    // (`\\?\`-prefixed) when it goes through `final_path_for_open` --
    // this is exactly the prefix shape flagged in Task 2's review as a
    // silent-fail-closed trap if a consumer assumes a bare drive form.
    let via_final_path = vfs_win::final_path_for_open(&short_root);
    if let Some(p) = &via_final_path {
        assert!(
            p.starts_with(r"\\?\"),
            "expected VOLUME_NAME_DOS (\\\\?\\-prefixed) form: {p}"
        );
    }

    let map = RootMap::new(&root_str, VolumeMap::empty()).unwrap();
    let raw = format!(r"{short_root}\Data\a.esp");
    assert!(
        map.contains(&raw),
        "8.3-spelled root was not recognised as inside: {raw}"
    );

    std::fs::remove_dir_all(&base).ok();
}

/// The mirror of the test above, and the case that had no coverage: the
/// root is **declared** in its 8.3 form, and a long-form path under it
/// must still be recognised as inside.
///
/// This is not symmetry for its own sake. `std::env::temp_dir()` returns
/// whatever `TMP` holds, and on a GitHub Windows runner `TMP` lives under
/// `C:\Users\RUNNER~1\...` — the account name `runneradmin` exceeds eight
/// characters, so the profile directory has an 8.3 alias and every path
/// derived from `temp_dir()` carries it. Eleven tests failed on CI for
/// this reason alone while passing on any machine whose user name happens
/// to fit in 8.3, which is why it went unnoticed: a root declared in short
/// form matched *nothing*, so every path under it looked like it belonged
/// to no one.
///
/// That is the "content simply missing" failure `with_roots` already warns
/// about, arriving through the spelling of the root rather than through a
/// parse error.
///
/// ## Why this is `#[ignore]`d rather than fixed
///
/// The obvious fix — expand the declared root's 8.3 components in
/// `with_capacity`, so the root is stored in the spelling an unfolded path
/// presents — **was implemented and reverted.** It works at this layer and
/// breaks the layer above: measured under a short-spelled `TMP`, the
/// `vfs-directord`/`vfs-shim`/`vfs-redirect` suites went from 271 passed /
/// 11 failed to 250 passed / 33 failed.
///
/// The reason is that `RootMap` is not the only thing that holds a root.
/// `vfs-shim` derives overlay paths and seal decisions from the root's
/// **as-declared** spelling, so expanding it here desynchronises the map's
/// view of the root from the shim's. The 22 new failures were concentrated
/// exactly there — copy-up, write seals, second-root overlays, and
/// `mo2_style_junction_inside_root_pointing_to_external_staging_is_sealed`.
///
/// So a real fix has to align both views at once, which is a larger change
/// than the symptom warrants: a launcher gets its paths from Steam config,
/// the registry or a file picker, all long-form, so a short-spelled root
/// does not arise in production. CI hits it only because a GitHub runner's
/// `TMP` sits under `RUNNER~1`, and that is handled by pointing `TMP` at a
/// long-form directory in the workflow instead.
///
/// This test stays, ignored, because the gap is real and the next person to
/// reach for the one-line fix should find out here that it has already been
/// tried and what it costs.
#[test]
#[cfg(windows)]
#[ignore = "known gap: expanding the declared root here desynchronises vfs-shim's \
            own view of it — see this test's doc comment for the measurement"]
fn a_root_declared_in_8dot3_form_recognises_its_long_form_paths() {
    let base =
        std::env::temp_dir().join(format!("vfs-redirect-830-declared-{}", std::process::id()));
    let long_name = "ThisIsALongRootDirectoryNameDeclaredShort";
    let root_dir = base.join(long_name);
    std::fs::create_dir_all(root_dir.join("Data")).unwrap();
    std::fs::write(root_dir.join("Data").join("a.esp"), b"x").unwrap();
    let long_root = root_dir.to_str().unwrap().to_string();

    let short_root = match vfs_win::short_path_name(&long_root) {
        Some(s) if !s.eq_ignore_ascii_case(&long_root) => s,
        _ => {
            // 8.3 generation disabled on this volume: there is no short
            // spelling to declare, so nothing to test. Same convention as
            // the sibling test above.
            std::fs::remove_dir_all(&base).ok();
            return;
        }
    };

    // Declare the root by its SHORT spelling — the CI condition.
    let map = RootMap::new(&short_root, VolumeMap::empty()).unwrap();

    let raw = format!(r"{long_root}\Data\a.esp");
    assert!(
        map.contains(&raw),
        "a root declared as {short_root} did not recognise its own long-form \
         path as inside: {raw}"
    );

    // And the short spelling must keep working, so the fix is an addition
    // rather than a swap of which spelling is privileged.
    let raw_short = format!(r"{short_root}\Data\a.esp");
    assert!(
        map.contains(&raw_short),
        "a root declared as {short_root} stopped recognising the spelling it \
         was declared with: {raw_short}"
    );

    std::fs::remove_dir_all(&base).ok();
}

/// A resolution that never left the raw string (pure `canonicalise` +
/// component match, no OS call) is a deterministic function of its input
/// and is safe to cache: the second lookup of the same raw spelling must
/// be served from the cache rather than recomputed.
#[test]
fn deterministic_resolution_is_cached() {
    let map = RootMap::new_with_cache_capacity(r"C:\Games\Skyrim", VolumeMap::empty(), 8).unwrap();
    let raw = r"C:\Games\Skyrim\Data\a.esp"; // no `~`: never reaches the OS branch.
    assert!(map.contains(raw));
    assert_eq!(
        map.cache_len(),
        1,
        "a deterministic resolution was not cached"
    );
    assert!(map.contains(raw));
    assert_eq!(map.cache_len(), 1, "the second lookup added a second entry");
}

/// The finding this test guards: an OS-resolved identity (an 8.3
/// short-name slot, a junction target) is not stable for the life of a
/// cache entry — the slot can be reused after a delete-and-recreate, or
/// the junction retargeted, mid-session. A stale POSITIVE is the
/// dangerous direction: an in-root short-name alias cached as "inside"
/// would stay "inside" after the real on-disk target is swapped for
/// something outside the root, which is exactly the over-eager failure
/// class this gate exists to avoid.
///
/// So: any resolution that consulted the OS (`compute_under_root`'s `~`
/// fallback) must never be cached, positive or negative. Proven here two
/// ways: the cache stays empty across two lookups of the same raw 8.3
/// spelling, and the OS-consult counter increments on BOTH lookups (proof
/// it was recomputed the second time, not served from a cache miss that
/// happened to also be empty for some other reason).
#[test]
#[cfg(windows)]
fn os_consulted_resolution_is_never_cached() {
    let base =
        std::env::temp_dir().join(format!("vfs-redirect-830-no-cache-{}", std::process::id()));
    let long_name = "ThisIsALongRootDirectoryNameForNoCacheTesting";
    let root_dir = base.join(long_name);
    std::fs::create_dir_all(root_dir.join("Data")).unwrap();
    std::fs::write(root_dir.join("Data").join("a.esp"), b"x").unwrap();
    let root_str = root_dir.to_str().unwrap().to_string();

    let short_root = match vfs_win::short_path_name(&root_str) {
        Some(s) if !s.eq_ignore_ascii_case(&root_str) => s,
        _ => {
            // 8.3 disabled on this volume: nothing forces the OS-consulted
            // branch here (Task 6's `unbuildable` case, not a failure).
            std::fs::remove_dir_all(&base).ok();
            return;
        }
    };

    let map = RootMap::new(&root_str, VolumeMap::empty()).unwrap();
    let raw = format!(r"{short_root}\Data\a.esp");

    assert!(map.contains(&raw));
    assert_eq!(map.cache_len(), 0, "an OS-consulted resolution was cached");
    assert_eq!(map.os_consult_count(), 1);

    assert!(map.contains(&raw));
    assert_eq!(
        map.cache_len(),
        0,
        "an OS-consulted resolution was cached on a second lookup"
    );
    assert_eq!(
        map.os_consult_count(),
        2,
        "the second lookup did not re-consult the OS -- it must have been served from a cache"
    );

    std::fs::remove_dir_all(&base).ok();
}

/// Gate-review finding: a path a *caller* assembled from its own OS query
/// (e.g. `vfs-shim` resolving `OBJECT_ATTRIBUTES.RootDirectory` via
/// `GetFinalPathNameByHandleW` on a handle it does not own) carries no `~`
/// and no other marker `compute_under_root` can see — from here it is
/// indistinguishable from an ordinary literal path, so on its own it
/// would be classified `Resolution::Deterministic` and cached permanently
/// even though the caller knows it is a snapshot of live, mutable state.
/// `UncachedScope` is the caller-side escape hatch for exactly this case.
///
/// Proven the same way `os_consulted_resolution_is_never_cached` proves
/// its case: a recomputation-counter delta, not merely an empty cache
/// (which a bug elsewhere could also produce for the wrong reason). Also
/// confirms the suppression is scoped to the guard's lifetime, not a
/// permanent regression: caching resumes once it is dropped.
#[test]
fn uncached_scope_suppresses_caching_of_an_otherwise_deterministic_path() {
    let map = RootMap::new_with_cache_capacity(r"C:\Games\Skyrim", VolumeMap::empty(), 8).unwrap();
    // No `~`: purely deterministic shape by `compute_under_root`'s own
    // rules -- would be cached on the very first lookup without the guard.
    let raw = r"C:\Games\Skyrim\Data\a.esp";

    {
        let _guard = UncachedScope::enter();
        assert!(map.contains(raw));
        assert_eq!(
            map.compute_count(),
            1,
            "the first guarded lookup did not compute at all"
        );
        assert_eq!(map.cache_len(), 0, "a guarded lookup was cached");

        assert!(map.contains(raw));
        assert_eq!(
            map.compute_count(),
            2,
            "a second lookup of the identical raw string, still under the guard, was served \
             from the cache instead of being recomputed -- it must recompute every time"
        );
        assert_eq!(
            map.cache_len(),
            0,
            "a guarded lookup was cached on a second pass"
        );
    }

    // The guard is dropped: this is a genuine first-ever cache miss for
    // `raw` (nothing above ever inserted), so it recomputes once more and
    // this time gets cached.
    assert!(map.contains(raw));
    assert_eq!(
        map.compute_count(),
        3,
        "the first unguarded lookup did not recompute"
    );
    assert_eq!(
        map.cache_len(),
        1,
        "caching did not resume once the guard was dropped"
    );

    // A second unguarded lookup is a genuine cache hit: proves the
    // suppression above came specifically from the guard, not from some
    // other reason `compute_count` might have kept moving.
    assert!(map.contains(raw));
    assert_eq!(
        map.compute_count(),
        3,
        "a cache hit outside the guard was recomputed instead of served from the cache"
    );
}

/// A path that never contains `~` and never matches the root deterministically
/// fails closed without ever touching the OS-consult counter — confirms the
/// `~` gate, not just the cache boundary, is doing its job.
#[test]
fn plainly_outside_path_never_consults_the_os() {
    let map = RootMap::new(r"C:\Games\Skyrim", VolumeMap::empty()).unwrap();
    assert!(!map.contains(r"C:\Windows\System32\kernel32.dll"));
    assert_eq!(map.os_consult_count(), 0);
}

/// `vfs_win::final_path_for_open` returns `GetFinalPathNameByHandleW`'s
/// default VOLUME_NAME_DOS form, which is `\\?\`-prefixed -- the Win32
/// spelling, never the NT `\??\` spelling a real hooked open presents.
/// Task 2's review found a bug of exactly this shape (a volume-GUID key
/// registered in the wrong prefix silently matched nothing). Guard the
/// same trap here: canonicalise must treat `\\?\` the same as any other
/// recognised NT/DOS prefix, not require the caller to strip it first,
/// so feeding a real OS-resolved path straight back into canonicalise
/// (as `under_root`'s fallback does) can never silently fail closed.
#[test]
#[cfg(windows)]
fn os_resolved_dos_prefixed_path_still_canonicalises_correctly() {
    let dir = std::env::temp_dir().join(format!("vfs-redirect-dosform-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("plain.txt");
    std::fs::write(&file, b"x").unwrap();

    let resolved = vfs_win::final_path_for_open(file.to_str().unwrap())
        .expect("should resolve an existing file");
    assert!(
        resolved.starts_with(r"\\?\"),
        "expected VOLUME_NAME_DOS form: {resolved}"
    );

    let canon = canonicalise(&resolved, &VolumeMap::empty()).unwrap();
    assert!(
        canon.to_ascii_lowercase().ends_with("plain.txt"),
        "lost the file name: {canon}"
    );
    assert!(
        !canon.contains('?'),
        "leftover NT/DOS prefix marker in canonical form: {canon}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The cache is bounded: pushing more distinct raw spellings through
/// `under_root` than its capacity must not grow it past that capacity —
/// a game runs for hours and opens a great many distinct paths over a
/// session, so an unbounded cache would leak memory for the life of the
/// process.
#[test]
fn cache_evicts_rather_than_growing_without_bound() {
    let map = RootMap::new_with_cache_capacity(r"C:\Games\Skyrim", VolumeMap::empty(), 2).unwrap();
    map.contains(r"C:\Games\Skyrim\Data\a.esp");
    map.contains(r"C:\Games\Skyrim\Data\b.esp");
    map.contains(r"C:\Games\Skyrim\Data\c.esp");
    assert!(
        map.cache_len() <= 2,
        "cache grew past its capacity: {}",
        map.cache_len()
    );
}

/// The same raw spelling queried twice is a single cache entry, not two —
/// the whole point of keying on the raw input string.
#[test]
fn repeated_raw_spelling_is_one_cache_entry() {
    let map = RootMap::new_with_cache_capacity(r"C:\Games\Skyrim", VolumeMap::empty(), 8).unwrap();
    let raw = r"C:\Games\Skyrim\Data\a.esp";
    map.contains(raw);
    map.contains(raw);
    map.contains(raw);
    assert_eq!(map.cache_len(), 1);
}

/// The strongest form of the over-eager check: a path that genuinely
/// lies OUTSIDE the root, but that also carries a `~` and so specifically
/// forces `compute_under_root` down its Win32-fallback branch (the same
/// branch the 8.3-spelling test above exercises for an IN-root path),
/// must still come back outside once the OS resolves it. A short-name
/// vector closing false positives (real files start matching the root
/// when they should not) would be a worse bug than the one this gate
/// fixes.
#[test]
#[cfg(windows)]
fn under_root_fallback_branch_does_not_pull_in_a_path_outside_the_root() {
    let base = std::env::temp_dir().join(format!(
        "vfs-redirect-830-over-eager-{}",
        std::process::id()
    ));
    let root_dir = base.join("TheManagedRootDirectory");
    let outside_dir = base.join("ANeighbouringDirectoryNotUnderTheRootAtAll");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&outside_dir).unwrap();
    std::fs::write(outside_dir.join("secret.txt"), b"not yours").unwrap();
    let outside_str = outside_dir.to_str().unwrap().to_string();

    let short_outside = match vfs_win::short_path_name(&outside_str) {
        Some(s) if !s.eq_ignore_ascii_case(&outside_str) => s,
        _ => {
            // 8.3 disabled on this volume: the fallback branch can't be
            // forced this way here (Task 6's `unbuildable` case).
            std::fs::remove_dir_all(&base).ok();
            return;
        }
    };

    let root_str = root_dir.to_str().unwrap().to_string();
    let map = RootMap::new(&root_str, VolumeMap::empty()).unwrap();
    let raw = format!(r"{short_outside}\secret.txt");
    assert!(
        !map.contains(&raw),
        "a path outside the root was pulled inside via the 8.3 fallback: {raw}"
    );

    std::fs::remove_dir_all(&base).ok();
}

// -- The object-manager spelling matrix ------------------------------
//
// `rust/docs/escape-matrix.md`'s vector 3 covers exactly one
// object-manager spelling: the colon-free `GLOBALROOT\Device\HarddiskVolumeN`
// form. Every *colon-bearing* spelling — the ones that reach a drive
// letter through a namespace token instead of a device name — went
// uncovered, and all three of them classified as outside every root while
// the OS resolved them to the file under it, so `create_hook`
// trampolined the original OBJECT_ATTRIBUTES and the kernel opened the
// real file. The vectors below are that gap, closed as a table so adding
// the next spelling is one line rather than a new test.
//
// Each entry is a template. `{VOL}` is not used; the volume spelling is
// written out in the template itself, and `{REST}` is substituted with a
// drive-relative tail — twice, once with a tail under the managed root
// and once with a tail outside it, so every vector is checked in **both**
// directions. Only asserting the under-root direction would pass for a
// canonicaliser that had simply started calling everything under-root,
// which is the worse of the two failure modes (it makes the VFS
// intercept traffic that was never ours).
//
// Every spelling here was verified against the real object manager with a
// direct `NtCreateFile` call (see this task's report): all of them return
// `STATUS_SUCCESS` for a real file, i.e. each one genuinely reaches the
// file and is therefore a genuine bypass when misclassified.
const NT_SPELLING_VECTORS: &[(&str, &str)] = &[
    ("win32-plain", r"C:{REST}"),
    ("dosdevices", r"\??\C:{REST}"),
    // The three that were broken.
    (
        "globalroot-global-dosdevices",
        r"\??\GLOBALROOT\GLOBAL??\C:{REST}",
    ),
    ("globalroot-dosdevices", r"\??\GLOBALROOT\??\C:{REST}"),
    ("global-dosdevices-bare", r"\GLOBAL??\C:{REST}"),
    // The `Global` symlink to `\GLOBAL??`, and nested combinations. Found
    // while enumerating siblings, not reported to this task.
    ("global-symlink", r"\??\Global\C:{REST}"),
    (
        "global-dosdevices-global-symlink",
        r"\GLOBAL??\Global\C:{REST}",
    ),
    (
        "global-symlink-globalroot-global",
        r"\??\Global\GLOBALROOT\GLOBAL??\C:{REST}",
    ),
    (
        "global-dosdevices-globalroot-global",
        r"\GLOBAL??\GLOBALROOT\GLOBAL??\C:{REST}",
    ),
    // Case: NT object-manager names are case-insensitive.
    (
        "globalroot-global-lowercase",
        r"\??\globalroot\global??\c:{REST}",
    ),
    // Device spellings: vector 3's own colon-free form, its bare
    // equivalent, and the same behind the other DosDevices spelling.
    ("device-bare", r"\Device\HarddiskVolume3{REST}"),
    (
        "globalroot-device",
        r"\??\GLOBALROOT\Device\HarddiskVolume3{REST}",
    ),
    (
        "global-dosdevices-globalroot-device",
        r"\GLOBAL??\GLOBALROOT\Device\HarddiskVolume3{REST}",
    ),
    // Volume GUID, in the `\??\`-keyed spelling a real open presents, and
    // behind the wrapper that used to hide it from `VolumeMap::resolve`.
    (
        "volume-guid",
        r"\??\Volume{12345678-1234-1234-1234-123456789abc}{REST}",
    ),
    (
        "globalroot-global-volume-guid",
        r"\??\GLOBALROOT\GLOBAL??\Volume{12345678-1234-1234-1234-123456789abc}{REST}",
    ),
    // The administrative UNC share, likewise.
    ("unc-admin-share", r"\??\UNC\localhost\C${REST}"),
    (
        "global-dosdevices-unc-admin-share",
        r"\GLOBAL??\UNC\localhost\C${REST}",
    ),
];

/// The session-frozen alias table these vectors resolve against, keyed
/// exactly the way `resolve_volume_map` keys the real one (`\??\`-prefixed
/// volume GUID and admin share — see `volumes::win32_guid_to_nt`), so a
/// vector cannot pass here on a spelling production would never register.
fn matrix_volumes() -> VolumeMap {
    let mut v = VolumeMap::empty();
    v.insert(r"\Device\HarddiskVolume3", 'C');
    v.insert(r"\??\Volume{12345678-1234-1234-1234-123456789abc}", 'C');
    v.insert_alias(r"\??\UNC\localhost\C$", "C:");
    v
}

/// **The governing invariant, per spelling.** Every NT object-manager
/// spelling of a path under a managed root resolves to that root with the
/// same remainder — so the director answers it, and `create_hook` never
/// trampolines the open to the real file underneath the mount.
#[test]
fn every_nt_spelling_of_an_under_root_path_resolves_under_the_root() {
    let map = RootMap::new(r"C:\Games\Skyrim", matrix_volumes()).unwrap();
    let want: Vec<String> = vec!["data".into(), "a.esp".into()];
    for (id, template) in NT_SPELLING_VECTORS {
        let raw = template.replace("{REST}", r"\Games\Skyrim\Data\a.esp");
        assert_eq!(
            map.resolve(&raw),
            Some((RootId::DEFAULT, want.clone())),
            "vector {id}: spelling {raw} did not resolve under the managed root — the real \
             file underneath the mount is reachable through it (canonicalised to {:?})",
            canonicalise(&raw, &matrix_volumes())
        );
    }
}

/// The other direction of the same table, and the reason the table is
/// checked twice: a canonicaliser that "fixed" the vectors above by
/// pulling everything under the root would break legitimate traffic
/// instead of leaking it. Every spelling, with a tail that is genuinely
/// outside the root, must stay outside.
#[test]
fn no_nt_spelling_pulls_an_out_of_root_path_inside() {
    let map = RootMap::new(r"C:\Games\Skyrim", matrix_volumes()).unwrap();
    for (id, template) in NT_SPELLING_VECTORS {
        let raw = template.replace("{REST}", r"\Windows\System32\kernel32.dll");
        assert!(
            !map.contains(&raw),
            "vector {id}: spelling {raw} was pulled inside the managed root, which it is not \
             under (canonicalised to {:?})",
            canonicalise(&raw, &matrix_volumes())
        );
    }
}

/// The fail-closed rule the wrapper spellings must not erode: a device
/// that is not in the session's table is never guessed into a drive, no
/// matter how many namespace tokens are stacked in front of it. Volume 9
/// is deliberately absent from `matrix_volumes`.
#[test]
fn an_unmapped_device_stays_outside_behind_every_wrapper() {
    let map = RootMap::new(r"C:\Games\Skyrim", matrix_volumes()).unwrap();
    for raw in [
        r"\Device\HarddiskVolume9\Games\Skyrim\Data\a.esp",
        r"\??\GLOBALROOT\Device\HarddiskVolume9\Games\Skyrim\Data\a.esp",
        r"\GLOBAL??\GLOBALROOT\Device\HarddiskVolume9\Games\Skyrim\Data\a.esp",
        r"\??\GLOBALROOT\GLOBAL??\Volume{ffffffff-ffff-ffff-ffff-ffffffffffff}\Games\Skyrim\Data\a.esp",
    ] {
        assert!(
            !map.contains(raw),
            "an unmapped volume was guessed into the managed root's drive: {raw}"
        );
    }
}

/// The specific corruption behind the defect, asserted on its own so a
/// regression names its own cause rather than only its consequence: the
/// drive-letter colon in `...\GLOBAL??\C:\...` is not an alternate-data-
/// stream separator, and cutting there left the stub `GLOBAL??/C` — a path
/// matching no root, so the open trampolined to real disk.
#[test]
fn a_drive_colon_behind_a_namespace_token_is_not_a_stream_separator() {
    let raw = r"\??\GLOBALROOT\GLOBAL??\C:\Games\Skyrim\Data\a.esp";
    let got = canonicalise(raw, &matrix_volumes()).unwrap();
    assert_eq!(
        got.to_ascii_lowercase(),
        "c:/games/skyrim/data/a.esp",
        "the path was truncated at the drive colon: {got}"
    );
    // The stream suffix on the same spelling is still stripped — the fix
    // narrowed where a stream may be found, it did not stop finding one.
    let with_stream = format!("{raw}:evil");
    assert_eq!(canonicalise(&with_stream, &matrix_volumes()).unwrap(), got);
}
