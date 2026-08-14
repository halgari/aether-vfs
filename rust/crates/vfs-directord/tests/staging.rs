//! Gate-3 task 1: the launch path stages the game EXE (and, for a launcher,
//! its spawn target) onto real disk so `CreateProcess`/the loader can find
//! them before the shim exists (see `vfs_director::stage`). Once the managed
//! root goes fully virtual, a real file under it that no provider serves is
//! invisible. These tests prove the provider graph — not disk passthrough —
//! answers `getattr`/`open` for every staged artifact, and that real game
//! content still wins over the staged copy where both could serve a path.

use std::collections::HashMap;

use vfs_control::SourceSpec;
use vfs_director::stage::ImageSource;
use vfs_director::KIND_FILE;
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
    reg.add_source(&summary.id, "/", 0, content_be).unwrap();

    // Baseline: the launcher (`skse64_loader.exe`) is not part of the game
    // archive at all, and nothing has staged it yet, so the provider graph
    // must not see it before `stage_launch` runs.
    reg.with_session_mut(&summary.id, |live| {
        assert!(
            live.session
                .kernel()
                .getattr("skse64_loader.exe")
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

    let stage_root = tempfile::tempdir().unwrap();
    let also = ["SkyrimSE.exe"];
    let staged = reg
        .stage_launch(
            &summary.id,
            &source,
            &vfs_directord::StageLaunchOpts {
                exe_vpath: "skse64_loader.exe",
                also: &also,
                stage_root: stage_root.path(),
                tag: "t1",
                fallback_dirs: &[],
            },
        )
        .expect("stage_launch");
    assert!(staged.exe().is_file(), "CreateProcess still needs a real on-disk image");

    // The launcher: reachable ONLY via staging — no content provider ever
    // served it. This is exactly what would go invisible once the
    // under-root passthrough is removed, and is what failed before this fix
    // (the baseline assertion above, run against the pre-fix code path,
    // stayed `None` forever with no `stage_launch` to change it).
    reg.with_session_mut(&summary.id, |live| {
        let st = live
            .session
            .kernel()
            .getattr("skse64_loader.exe")
            .unwrap()
            .expect("staged loader must resolve through the provider graph, not passthrough");
        assert_eq!(st.kind, KIND_FILE);
        let (fh, size, is_dir) = live
            .session
            .kernel()
            .open("skse64_loader.exe", vfs_director::OPEN_READ)
            .expect("open must succeed through the provider graph");
        assert!(!is_dir);
        assert_eq!(size, st.size);
        let _ = live.session.kernel().close(fh);
        let bytes = live.session.read_file("skse64_loader.exe").unwrap();
        assert_eq!(bytes, bare_pe(b"LOADER"));
        Ok(())
    })
    .unwrap();

    // The spawn target: content already serves `SkyrimSE.exe` at the same
    // path the staging provider now also covers. Real content must win.
    reg.with_session_mut(&summary.id, |live| {
        assert!(live.session.kernel().getattr("SkyrimSE.exe").unwrap().is_some());
        let bytes = live.session.read_file("SkyrimSE.exe").unwrap();
        assert_eq!(
            bytes,
            bare_pe(b"REAL-CONTENT"),
            "game content must win over the staging layer on a shared path"
        );
        Ok(())
    })
    .unwrap();

    drop(staged);
}
