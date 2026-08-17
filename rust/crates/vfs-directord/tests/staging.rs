//! Gate-3 task 1: the launch path stages the game EXE (and, for a launcher,
//! its spawn target) onto real disk so `CreateProcess`/the loader can find
//! them before the shim exists (see `vfs_director::stage`). Once the managed
//! root goes fully virtual, a real file under it that no provider serves is
//! invisible. These tests prove the provider graph — not disk passthrough —
//! answers `getattr`/`open` for every staged artifact, and that real game
//! content still wins over the staged copy where both could serve a path.

use std::collections::HashMap;
use std::path::Path;

use vfs_control::SourceSpec;
use vfs_director::stage::ImageSource;
use vfs_embed::{LaunchOpts, RootId, StageOpts, KIND_FILE};
use vfs_directord::SessionRegistry;
use vfs_source::build_provider;

/// Minimal PE: MZ header, e_lfanew, PE32+ optional header, no imports —
/// exactly what `vfs_director::stage`'s own tests use. A trailing marker
/// distinguishes which copy of a same-named file actually answered a read.
fn bare_pe(marker: &[u8]) -> Vec<u8> {
    let mut pe = vec![0u8; 0x400];
    pe[0] = b'M';
    pe[1] = b'Z';
    pe[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
    pe[0x80..0x84].copy_from_slice(b"PE\0\0");
    pe[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
    pe[0x94..0x96].copy_from_slice(&240u16.to_le_bytes());
    pe[0x98..0x9A].copy_from_slice(&0x20Bu16.to_le_bytes());
    pe.extend_from_slice(marker);
    pe
}

struct FakeSource(HashMap<String, Vec<u8>>);
impl ImageSource for FakeSource {
    fn read(&self, vpath: &str) -> Option<Vec<u8>> {
        self.0.get(vpath).cloned()
    }
}

#[test]
fn staged_launch_artifacts_resolve_through_the_provider_graph() {
    // The game's own content already carries `SkyrimSE.exe` — a real mount,
    // just like the zip layer in production. Its bytes are deliberately
    // different from the staged copy below so a later assertion can tell
    // which one actually answered.
    let content_dir = tempfile::tempdir().unwrap();
    std::fs::write(content_dir.path().join("SkyrimSE.exe"), bare_pe(b"REAL-CONTENT")).unwrap();

    let reg = SessionRegistry::new();
    let summary = reg.create("stage-test".into()).unwrap();
    let content_be = build_provider(&SourceSpec::Disk {
        path: content_dir.path().to_string_lossy().into_owned(),
    })
    .unwrap();
    reg.add_source(&summary.id, 0, "/", 0, content_be).unwrap();

    // Baseline: the launcher (`skse64_loader.exe`) is not part of the game
    // archive at all, and nothing has staged it yet, so the provider graph
    // must not see it before `stage_launch` runs.
    reg.with_session_mut(&summary.id, |live| {
        assert!(
            live.session
                .kernel()
                .getattr(RootId::DEFAULT, "skse64_loader.exe")
                .unwrap()
                .is_none(),
            "must not be visible before staging"
        );
        Ok(())
    })
    .unwrap();

    // What a real launch stages: the loader plus its spawn target, sourced
    // independently of session content (mirrors `stage_launch_with` being
    // fed from the session's VFS in production — here a fake source keeps
    // the test focused on composition, not PE parsing).
    let mut m = HashMap::new();
    m.insert("skse64_loader.exe".to_string(), bare_pe(b"LOADER"));
    m.insert("SkyrimSE.exe".to_string(), bare_pe(b"STAGED-COPY"));
    let source = FakeSource(m);

    // Task 4b moved this sequence into `vfs_embed::Session`, so the daemon
    // drives it through the session rather than owning a second copy — the
    // staging directory, the `StagedDir`'s lifetime and the below-everything
    // mount are all the session's now.
    let also = ["SkyrimSE.exe"];
    let staged_exe = reg
        .with_session_mut(&summary.id, |live| {
            live.session.stage_launch(
                &source,
                &StageOpts {
                    exe_vpath: "skse64_loader.exe",
                    also: &also,
                    fallback_dirs: &[],
                },
            )
        })
        .expect("stage_launch");
    assert!(staged_exe.is_file(), "CreateProcess still needs a real on-disk image");

    // The launcher: reachable ONLY via staging — no content provider ever
    // served it. This is exactly what would go invisible once the
    // under-root passthrough is removed, and is what failed before this fix
    // (the baseline assertion above, run against the pre-fix code path,
    // stayed `None` forever with no `stage_launch` to change it).
    reg.with_session_mut(&summary.id, |live| {
        let st = live
            .session
            .kernel()
            .getattr(RootId::DEFAULT, "skse64_loader.exe")
            .unwrap()
            .expect("staged loader must resolve through the provider graph, not passthrough");
        assert_eq!(st.kind, KIND_FILE);
        let (fh, size, is_dir) = live
            .session
            .kernel()
            .open(RootId::DEFAULT, "skse64_loader.exe", vfs_director::OPEN_READ)
            .expect("open must succeed through the provider graph");
        assert!(!is_dir);
        assert_eq!(size, st.size);
        let _ = live.session.kernel().close(fh);
        let bytes = live.session.read_file("skse64_loader.exe").unwrap();
        assert_eq!(bytes, bare_pe(b"LOADER"));
        // vfs-redirect Task 4 deleted the shim-side local merge that used to
        // paper over an under-root real file's enumeration — a directory
        // listing under a fully virtual root is now the director's `readdir`
        // alone. Confirm the staged loader — reachable only via staging, per
        // the comment above — actually enumerates through the provider
        // graph, not merely answers `getattr`/`open` individually.
        let listed = live.session.kernel().readdir(RootId::DEFAULT, "").unwrap();
        assert!(
            listed.iter().any(|e| e.name == "skse64_loader.exe"),
            "staged loader did not enumerate through the provider graph: {:?}",
            listed.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        Ok(())
    })
    .unwrap();

    // The spawn target: content already serves `SkyrimSE.exe` at the same
    // path the staging provider now also covers. Real content must win.
    reg.with_session_mut(&summary.id, |live| {
        assert!(live.session.kernel().getattr(RootId::DEFAULT, "SkyrimSE.exe").unwrap().is_some());
        let bytes = live.session.read_file("SkyrimSE.exe").unwrap();
        assert_eq!(
            bytes,
            bare_pe(b"REAL-CONTENT"),
            "game content must win over the staging layer on a shared path"
        );
        Ok(())
    })
    .unwrap();
}

/// GAP 1 closed: `SessionRegistry::launch` — the exact function
/// `DirectorService::launch` calls, i.e. the real `vfs launch --exec` /
/// scenario-TOML production path — must stage a relative launch image and
/// serve it through the provider graph, with the caller doing nothing but
/// naming the vpath.
///
/// Since Task 4b the daemon no longer implements that itself; it delegates to
/// `vfs_embed::Session::launch`. This test is deliberately unchanged in what
/// it drives and asserts, because "the daemon still does the same thing" is
/// precisely the claim a relocation has to keep making.
///
/// The staged bytes here are not a runnable Windows image (same minimal
/// `bare_pe` as above), so `Session::launch`'s later `CreateProcess`/shim
/// steps are expected to fail — that failure is not what this test is
/// about. What matters is what already happened *before* that failure:
/// staging and provider-graph mounting, driven by `launch()` itself with no
/// direct call to `stage_launch` from the test.
///
/// Disambiguation matters here: the content provider that seeds staging
/// already serves `game.exe` at the same vpath, so `getattr`/`open`
/// succeeding by itself would not prove staging ran — it could just be
/// content answering as it always would. So the content copy is deleted
/// from disk *after* `launch()` returns and *before* the provider-graph
/// check: if `launch()` never staged the image, its only backing file is
/// now gone and resolution must fail; if it did stage it, the *staging*
/// provider's independent on-disk copy (a different `DiskProvider`, over a
/// different directory) is what has to answer.
#[test]
fn production_launch_stages_a_relative_image_before_create_process() {
    let content_dir = tempfile::tempdir().unwrap();
    let content_exe = content_dir.path().join("game.exe");
    std::fs::write(&content_exe, bare_pe(b"CONTENT")).unwrap();

    let reg = SessionRegistry::new();
    let summary = reg.create("launch-wiring-test".into()).unwrap();
    let content_be = build_provider(&SourceSpec::Disk {
        path: content_dir.path().to_string_lossy().into_owned(),
    })
    .unwrap();
    reg.add_source(&summary.id, 0, "/", 0, content_be).unwrap();

    // `image` is a *relative* vpath — the production shape (`--exec
    // SkyrimSE.exe`, `[launch] exec = "..."`), not an already-staged disk
    // path. The session's root is empty on disk, so the only way this can
    // reach `CreateProcess` at all is for the launch path to stage it out of
    // the graph first.
    let err = reg
        .launch(
            &summary.id,
            LaunchOpts {
                image: "game.exe".into(),
                wait: true,
                ..Default::default()
            },
        )
        .expect_err("the fake PE cannot actually run — CreateProcess/shim setup must fail");
    assert!(!err.is_empty());

    // Remove content's only backing file (see doc comment above).
    std::fs::remove_file(&content_exe).unwrap();

    reg.with_session_mut(&summary.id, |live| {
        let st = live
            .session
            .kernel()
            .getattr(RootId::DEFAULT, "game.exe")
            .unwrap()
            .expect(
                "launch() must stage the relative image through the provider graph — \
                 content's own backing file is gone, so only a staging mount can answer",
            );
        assert_eq!(st.kind, KIND_FILE);
        let (fh, size, is_dir) = live
            .session
            .kernel()
            .open(RootId::DEFAULT, "game.exe", vfs_director::OPEN_READ)
            .expect("open must succeed through the provider graph after launch() staged it");
        assert!(!is_dir);
        assert_eq!(size, st.size);
        let _ = live.session.kernel().close(fh);
        Ok(())
    })
    .unwrap();
}

/// An absolute `image` (an already-staged path, or a fixture binary that was
/// never VFS content) must pass through untouched — no VFS lookup, matching
/// `Session::launch`'s own absolute/relative split. Proven by pointing at a
/// real file the session's content never claims to serve.
#[test]
fn production_launch_leaves_an_absolute_image_untouched() {
    let outside = tempfile::tempdir().unwrap();
    let exe = outside.path().join("already-staged.exe");
    std::fs::write(&exe, bare_pe(b"PRESTAGED")).unwrap();
    assert!(Path::new(&exe).is_absolute());

    let reg = SessionRegistry::new();
    let summary = reg.create("absolute-image-test".into()).unwrap();
    // No content mounted at all — an absolute image must not need any.

    let err = reg
        .launch(
            &summary.id,
            LaunchOpts {
                image: exe.to_string_lossy().into_owned(),
                wait: true,
                ..Default::default()
            },
        )
        .expect_err("the fake PE cannot actually run");
    assert!(!err.is_empty());

    // No staging happened, so the provider graph — which has nothing
    // mounted — still knows nothing about this path. Confirms staging was
    // correctly skipped rather than silently failing to serve it.
    reg.with_session_mut(&summary.id, |live| {
        assert!(live.session.kernel().getattr(RootId::DEFAULT, "already-staged.exe").unwrap().is_none());
        Ok(())
    })
    .unwrap();
}
