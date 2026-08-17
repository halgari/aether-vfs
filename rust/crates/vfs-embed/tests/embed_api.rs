//! The seam test: a whole session — roots, a composed provider graph, a
//! write, and a read-back — built through `vfs_embed`'s public API and
//! **nothing else**.
//!
//! The interesting assertion is not any single `assert_eq!` below; it is the
//! import list. This file names no engine crate. If a host cannot do what the
//! daemon does without reaching past `vfs-embed`, the crate is not the seam
//! spec §4 says it is, and the second host (the Node binding) would have
//! discovered that instead of this test. `no_engine_crate_is_named_here`
//! makes that literal by reading this file back.

use std::sync::Arc;

use vfs_embed::{
    rejected_writes, reset_rejected_writes, DiskProvider, InlineProvider, LaunchOpts, RootId,
    RootSources, Session, OPEN_READ, OPEN_WRITE,
};

/// A scratch directory holding one file, unique per test line.
fn dir(tag: &str, files: &[(&str, &[u8])]) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("vfs-embed-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    for (name, bytes) in files {
        std::fs::write(p.join(name), bytes).unwrap();
    }
    p
}

fn read_whole(session: &Session, root: RootId, rel: &str) -> Vec<u8> {
    let kernel = session.kernel();
    let (fh, size, is_dir) = kernel.open(root, rel, OPEN_READ).expect("open for read");
    assert!(!is_dir, "{rel} must be a file");
    let mut buf = vec![0u8; size as usize];
    let mut off = 0usize;
    while off < buf.len() {
        match kernel.read(fh, off as u64, &mut buf[off..]).unwrap() {
            0 => break,
            n => off += n,
        }
    }
    kernel.close(fh).unwrap();
    buf.truncate(off);
    buf
}

/// Everything a host does, in the order it does it: declare roots, compose a
/// graph per root, set the writable upper, serve the ring, write through the
/// director, read it back.
///
/// Two roots, because one root hides the whole class of bug this session type
/// exists to prevent: a single-root session cannot show that root 1's write
/// lands in root 1's write layer rather than root 0's.
#[test]
fn a_two_root_session_composes_writes_and_reads_back_through_vfs_embed_alone() {
    let base = dir("base", &[("shared.txt", b"BASE"), ("only-base.txt", b"KEEP")]);
    let mods = dir("mods", &[("shared.txt", b"MOD-WINS")]);
    let tools = dir("tools", &[("t.exe", b"TOOL")]);
    let upper0 = dir("upper0", &[]);
    let docs = dir("docs", &[("Skyrim.ini", b"ORIGINAL-INI")]);
    let upper1 = dir("upper1", &[]);
    let session_base = dir("session", &[]);

    let mut session = Session::new();
    session.set_root(session_base.join("root"));
    session.set_overlay(session_base.join("overlay"));
    session.set_state_dir(session_base.join("state"));

    // Root 1 is a real host directory the injected process must recognise as
    // root 1 — declaring it is separate from mounting anything on it.
    session.declare_root(1, &docs);
    assert_eq!(
        session.declared_roots().len(),
        1,
        "the declaration must reach the session, not be parsed and dropped"
    );

    // Root 0's graph, built the way a host that learns its sources one at a
    // time builds it: two layered sources plus one prefixed sibling.
    let mut root0 = RootSources::new();
    root0.add("/", 0, Arc::new(DiskProvider::new(&base)));
    root0.add("/", 10, Arc::new(DiskProvider::new(&mods)));
    root0.add("/Tools", 0, Arc::new(DiskProvider::new(&tools)));
    session
        .set_root_mounts(RootId::DEFAULT, root0.mounts().expect("compose root 0"))
        .expect("mount root 0");
    session
        .set_write_layer(Arc::new(DiskProvider::new(&upper0)))
        .expect("root 0 write layer");

    // Root 1's graph, built the single-source way.
    session
        .mount_at(RootId(1), "", Arc::new(DiskProvider::new(&docs)))
        .expect("mount root 1");
    session
        .set_write_layer_at(RootId(1), Arc::new(DiskProvider::new(&upper1)))
        .expect("root 1 write layer");

    assert_eq!(
        session.composed_roots(),
        vec![RootId::DEFAULT, RootId(1)],
        "both roots must be enumerable through the API, ascending"
    );
    assert!(session.has_write_layer(RootId::DEFAULT));
    assert!(session.has_write_layer(RootId(1)));

    // The ring the injected shim would attach to. A host that cannot start
    // this through the embeddable API cannot launch anything.
    session.serve().expect("serve");
    assert!(session.is_serving());
    assert!(session.ipc().is_some(), "the live ring must be inspectable");

    // Reads: the layer stack merges (later layer wins per entry, lower-layer
    // exclusives survive) and the prefixed sibling keeps its own subtree.
    assert_eq!(read_whole(&session, RootId::DEFAULT, "shared.txt"), b"MOD-WINS");
    assert_eq!(read_whole(&session, RootId::DEFAULT, "only-base.txt"), b"KEEP");
    assert_eq!(read_whole(&session, RootId::DEFAULT, "Tools/t.exe"), b"TOOL");
    // Same relative name, different root, different bytes.
    assert_eq!(read_whole(&session, RootId(1), "Skyrim.ini"), b"ORIGINAL-INI");

    // The write. An in-place edit of content only a *source* holds is the
    // case a sibling mount cannot serve — it has to copy up into the write
    // layer first, which is what makes the composition an overlay.
    {
        let kernel = session.kernel();
        let (fh, size, _) = kernel
            .open(RootId(1), "Skyrim.ini", OPEN_WRITE)
            .expect("an in-place edit must copy up, not be refused");
        assert_eq!(size, 12, "the handle opens onto the copied-up content");
        kernel.write(fh, 0, b"EDITED-INI!!").unwrap();
        kernel.close(fh).unwrap();
    }

    // Read back through the director — the same graph the child process sees.
    assert_eq!(read_whole(&session, RootId(1), "Skyrim.ini"), b"EDITED-INI!!");
    assert_eq!(
        std::fs::read(upper1.join("Skyrim.ini")).ok(),
        Some(b"EDITED-INI!!".to_vec()),
        "the edit belongs in root 1's write layer"
    );
    assert_eq!(
        std::fs::read(docs.join("Skyrim.ini")).unwrap(),
        b"ORIGINAL-INI",
        "the source it copied from must be untouched"
    );
    assert!(
        !upper0.join("Skyrim.ini").exists(),
        "root 1's write must not land in root 0's write layer"
    );

    // The host-side convenience read, on root 0.
    assert_eq!(session.read_file("shared.txt").unwrap(), b"MOD-WINS");

    session.stop_serve();
    assert!(!session.is_serving());

    for p in [base, mods, tools, upper0, docs, upper1, session_base] {
        let _ = std::fs::remove_dir_all(p);
    }
}

/// A root with no write layer cannot copy up, and the refusal is
/// *discoverable* through this crate rather than only through the kernel's
/// counters (spec §7: launch, ask what was rejected, add an overlay).
///
/// Runs in the same binary as the test above but touches a process-wide
/// table, so it takes its own read-only root id and asserts on its own path
/// rather than on the table's size.
#[test]
fn a_refused_write_is_reported_through_the_embed_api() {
    reset_rejected_writes();

    let session = Session::new();
    // No write layer on this root, and the source itself declares `Read` — an
    // ordinary disk directory would happily take the write and prove nothing.
    session
        .mount_at(
            RootId(7),
            "",
            Arc::new(InlineProvider::from_files([(
                "locked.esp",
                b"READONLY".as_slice(),
            )])),
        )
        .expect("mount root 7");

    let err = session
        .kernel()
        .open(RootId(7), "locked.esp", OPEN_WRITE)
        .expect_err("a write with no ReadWrite provider must be refused");
    assert_eq!(err, vfs_embed::ST_READ_ONLY);

    assert!(
        rejected_writes().iter().any(|(p, n)| p.contains("locked.esp") && *n >= 1),
        "the refusal must be discoverable through vfs_embed: {:?}",
        rejected_writes()
    );
    assert!(
        session
            .rejected_writes()
            .iter()
            .any(|(p, _)| p.contains("locked.esp")),
        "the same table, reachable from the session"
    );

}

/// `LaunchOpts` is part of the surface, not something a host has to reach into
/// the kernel crate for.
#[test]
fn launch_opts_are_constructible_from_this_crate() {
    let opts = LaunchOpts {
        image: "SkyrimSE.exe".into(),
        wait: false,
        ..Default::default()
    };
    assert_eq!(opts.image, "SkyrimSE.exe");
    assert!(!opts.wait);
    assert!(opts.env.is_empty());
}

/// The assertion this whole file exists to make, made literal.
///
/// Everything above compiles against `vfs_embed` only — but "I did not import
/// the engine" is a claim about source text, and source text drifts. Reading
/// the file back is the only form of it that stays true. The needles are
/// assembled at compile time so that stating them here does not trip the
/// check.
#[test]
fn no_engine_crate_is_named_here() {
    let src = include_str!("embed_api.rs");
    for needle in [concat!("vfs_", "director"), concat!("vfs_", "directord")] {
        assert!(
            !src.contains(needle),
            "a host must be able to do all of this without naming `{needle}` — \
             if this file needs it, the missing piece belongs in vfs-embed, \
             not in the test"
        );
    }
}
