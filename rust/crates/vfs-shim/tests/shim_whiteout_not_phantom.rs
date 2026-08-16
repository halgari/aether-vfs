//! Gate 5, Task 7: a shim-written whiteout marker must not survive a
//! **director** listing — neither as an entry of its own, nor as a marker that
//! fails to hide the file it names.
//!
//! ## Why the fixture looks like this
//!
//! The shim and the director share one physical directory and spell whiteouts
//! differently. `Overlay::whiteout_path` appends `vfs_redirect::WHITEOUT_SUFFIX`
//! (`<name>.__vfs_wh__`); `vfs_compose::OverlayProvider` prefixes `.wh.<name>`.
//! Live, `skyrim-live`/`vfs-launch` hand the director a write layer rooted at
//! `overlay_layer_dir(overlay, RootId::DEFAULT)` — the *same* directory
//! `Engine`'s local overlay writes into. So a marker the shim wrote is, to the
//! director, a zero-byte file with an odd name, and `client.readdir` hands it
//! back like any other entry.
//!
//! That is why the fake serves the marker as an ordinary file rather than
//! filtering it: filtering it in the fixture would test a director that does
//! not exist. The two halves that follow are both consequences of that one
//! fact, and before this task both were wrong:
//!
//! 1. the marker itself surfaced to the game as a real file, and
//! 2. the file it names stayed listed, so the delete that wrote it came back.
//!
//! ## What this does *not* claim
//!
//! Only enumeration. A stale marker still does not hide its target from an
//! `open` through the director: `OverlayProvider::hidden_by_whiteout` looks for
//! its own `.wh.` spelling, and the shim has no per-open hook that could ask
//! without a `stat` on every read. The route that mints such a marker while a
//! director is live is `setinfo_hook`'s engine branch, which — unlike
//! `delete_hook` — never asks the client first; that divergence is recorded in
//! the task report, not fixed here.

mod fakedirector;
mod ntapi;

use vfs_redirect::RootId;
use vfs_shim::{install, overlay_layer_dir, Engine};

const KEPT: &[u8] = b"kept";
const GONE: &[u8] = b"gone";

/// One installed session for the whole binary — `install` is
/// initialise-once for both the fake and the shim's own detours.
fn session() -> &'static std::path::Path {
    static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(setup)
}

fn list(dir: &std::path::Path, wildcard: Option<&str>) -> Vec<String> {
    let (st, h) = ntapi::nt_open_dir_abs(&dir.to_string_lossy(), ntapi::FILE_LIST_DIRECTORY);
    assert_eq!(st, 0, "opening the director-served directory failed: {st:#x}");
    let names = ntapi::nt_enum_classic_filtered(h, wildcard);
    ntapi::close(h);
    names
}

/// Build a session whose director serves `data/` with a shim-spelled whiteout
/// marker sitting in it, exactly as a shared overlay directory produces.
fn setup() -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("vfs-shim-wh-phantom-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    let overlay = base.join("overlay");
    std::fs::create_dir_all(root.join("data")).unwrap();
    let layer = overlay_layer_dir(&overlay, RootId::DEFAULT);
    std::fs::create_dir_all(layer.join("data")).unwrap();

    // The marker physically present in the shared directory, written the way
    // `Overlay::whiteout` writes it. The director's write layer is a
    // `DiskProvider` over exactly this directory, so it reads it back as a
    // file — which is what the fake below reproduces.
    std::fs::write(layer.join("data").join(vfs_redirect::whiteout_marker("gone.esp")), b"")
        .unwrap();

    let marker = format!("data/{}", vfs_redirect::whiteout_marker("gone.esp"));
    fakedirector::install(
        &root,
        fakedirector::Fake::new()
            .with_dir("data")
            .with("data/keep.esp", KEPT.to_vec(), fakedirector::ReadStyle::Whole)
            .with("data/gone.esp", GONE.to_vec(), fakedirector::ReadStyle::Whole)
            .with(&marker, Vec::new(), fakedirector::ReadStyle::Whole)
            .writable_under("data/"),
        0,
    );

    let engine = Engine::with_overlay(
        root.to_str().unwrap(),
        overlay.to_str().unwrap(),
        vfs_shared::bridge::flatten(
            &vfs_core::build(vec![vfs_core::Layer {
                id: vfs_core::LayerId(0),
                entries: Vec::new(),
            }])
            .unwrap(),
        ),
    )
    .unwrap();
    std::mem::forget(install(engine).expect("install"));
    root
}

#[test]
fn a_shim_whiteout_is_neither_listed_nor_ineffective_in_a_director_listing() {
    let root = session();
    let names = list(&root.join("data"), None);

    assert!(
        names.iter().any(|n| n.eq_ignore_ascii_case("keep.esp")),
        "the director's own entries vanished — this fixture is not exercising the director \
         listing branch at all, so neither assertion below would mean anything: {names:?}"
    );

    let marker = vfs_redirect::whiteout_marker("gone.esp");
    assert!(
        !names.iter().any(|n| n.eq_ignore_ascii_case(&marker)),
        "half one: the whiteout marker surfaced to the caller as a real file called {marker}. \
         The director spells whiteouts `.wh.<name>` and has no reason to hide the shim's \
         spelling, so the shim's own listing branch has to: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.eq_ignore_ascii_case("gone.esp")),
        "half two: the marker hid nothing. A whiteout that leaves its target listed is a \
         delete that comes back on the next enumeration: {names:?}"
    );
}

/// The ordering constraint the fix depends on, made observable.
///
/// A game asks for `*.esp`, not for everything. `gone.esp.__vfs_wh__` does not
/// match `*.esp`, so a wildcard filter that runs *first* drops the marker on
/// its own merits and leaves `gone.esp` in the listing — the hiding half
/// silently lost for precisely the queries that matter most, while the
/// unfiltered test above still passes.
///
/// So this is the mutation check for `strip_whiteout_markers`' placement, not
/// a second flavour of the same assertion: move the call after the `retain` in
/// `serve_dir_query`'s director branch and only this test fails.
#[test]
fn the_marker_still_hides_its_target_under_a_wildcard_that_excludes_the_marker() {
    let root = session();
    let names = list(&root.join("data"), Some("*.esp"));

    assert!(
        names.iter().any(|n| n.eq_ignore_ascii_case("keep.esp")),
        "`*.esp` matched nothing at all, so the assertion below is vacuous: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.eq_ignore_ascii_case("gone.esp")),
        "the wildcard filter ran before the marker was consumed: `*.esp` rejects \
         `gone.esp.__vfs_wh__`, so the marker was dropped as a non-match and stopped hiding \
         anything: {names:?}"
    );
}
