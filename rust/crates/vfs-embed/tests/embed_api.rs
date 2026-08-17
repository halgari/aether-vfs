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
    RootSources, Session, OPEN_WRITE,
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

/// A whole-file read out of any root's graph.
///
/// This used to be a hand-rolled open/read/close loop against
/// `session.kernel()`, for one reason: `Session::read_file` hardcoded root 0, so
/// the only host-side way to read the second root a *two*-root session exists to
/// test was to bypass the accessor. `Session::read_file_at` is that gap closed,
/// and this helper now exists only to unwrap and keep the assertions short.
fn read_whole(session: &Session, root: RootId, rel: &str) -> Vec<u8> {
    session
        .read_file_at(root, rel)
        .unwrap_or_else(|st| panic!("read_file_at({root:?}, {rel}) status {st}"))
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

    // The host-side convenience reads. `read_file` is root 0 and
    // `read_file_at` is any root, and the pair is checked together here for a
    // specific reason. A `read_file_at` that ignored its argument *is* caught
    // above — it was, when mutated — but only because root 0 happens to hold no
    // `Skyrim.ini` at that point, so the read fails with `ST_NOT_FOUND` rather
    // than answering the wrong file. That is an accident of the fixture. Reading
    // the **same relative name** out of both roots and requiring different bytes
    // is the check that does not depend on it.
    assert_eq!(session.read_file("shared.txt").unwrap(), b"MOD-WINS");
    // `DiskProvider` reads live, so root 0 gains a file of the same name
    // without recomposing anything.
    std::fs::write(mods.join("Skyrim.ini"), b"ROOT-0-INI!").unwrap();
    assert_eq!(session.read_file("Skyrim.ini").unwrap(), b"ROOT-0-INI!");
    assert_eq!(
        session.read_file_at(RootId(1), "Skyrim.ini").unwrap(),
        b"EDITED-INI!!",
        "the root argument selects the graph, and root 0 has its own file of this name"
    );
    assert_eq!(
        session.read_file_at(RootId::DEFAULT, "Skyrim.ini").unwrap(),
        session.read_file("Skyrim.ini").unwrap(),
        "read_file is read_file_at(DEFAULT, ..) and nothing else"
    );
    // An unmounted root answers rather than panicking or reading root 0.
    assert!(
        session.read_file_at(RootId(9), "Skyrim.ini").is_err(),
        "a root with no graph must fail, not fall back to root 0"
    );

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

/// Declaring root 0 must **do** something.
///
/// It used to be accepted, recorded in `declared_roots()`, and then dropped on
/// the way to the environment the child inherits — the one outcome that cannot
/// be right, and invisible to a host that had every reason to believe the call
/// took. Root 0's host directory is the managed root, so that is where the
/// declaration goes; a host walking its roots and declaring all of them gets
/// what it asked for rather than a silent no-op on the first one.
#[test]
fn declaring_root_zero_repoints_the_managed_root_instead_of_being_discarded() {
    let game = dir("declare0-game", &[]);
    let docs = dir("declare0-docs", &[]);

    let mut session = Session::new();
    session.declare_root(0, &game);
    assert_eq!(
        session.virtual_root(),
        game.as_path(),
        "root 0 is the managed root; declaring it must move the managed root"
    );
    assert!(
        session.declared_roots().is_empty(),
        "root 0 has one home, not two — it must not also sit in the extra-roots list, \
         which is the list that gets published to the child"
    );

    // Re-declaring replaces, and the other roots are unaffected either way.
    session.declare_root(1, &docs);
    session.declare_root(0, docs.join("elsewhere"));
    assert_eq!(session.virtual_root(), docs.join("elsewhere").as_path());
    assert_eq!(session.declared_roots(), [(1u32, docs.clone())]);
}

/// A relative `LaunchOpts.image` is resolved on real disk under the managed
/// root, then — Task 4b — as a vpath in the provider graph, which is staged
/// out. Both halves of that are proven end to end in `launch_vfs_content.rs`,
/// including a process that actually runs from bytes only the graph held.
///
/// What is asserted here is the **diagnosis**, which is what a host meets when
/// it gets this wrong. The three outcomes have to stay distinguishable: an
/// image the graph serves but the stager cannot use, a name nothing serves at
/// all, and no name given. Collapsing any two of them sends a host chasing the
/// wrong thing.
#[test]
fn launching_a_relative_image_reports_which_of_the_three_ways_it_failed() {
    let content = dir("launch-content", &[("game.exe", b"MZ-not-really")]);
    let base = dir("launch-session", &[]);

    let mut session = Session::new();
    session.set_root(base.join("root"));
    session.set_overlay(base.join("overlay"));
    session.set_state_dir(base.join("state"));
    session
        .mount("", Arc::new(DiskProvider::new(&content)))
        .expect("mount content");
    session.serve().expect("serve");

    // The graph serves `game.exe`, so `launch` routes it to staging rather
    // than to `CreateProcess` — and the stager is what refuses it, because
    // those bytes are not a PE image. That the *stager* is the one complaining
    // is the evidence that the graph route was taken at all.
    let err = session
        .launch(&LaunchOpts {
            image: "game.exe".into(),
            wait: true,
            ..Default::default()
        })
        .expect_err("bytes that are not a PE image cannot be staged");
    assert!(
        err.contains("game.exe") && err.contains("not a PE"),
        "an image the graph holds must reach the stager, and the stager's own \
         complaint must survive: {err}"
    );

    // Nothing has that path at all — neither disk nor graph, so there is not
    // even anything to stage.
    let err = session
        .launch(&LaunchOpts {
            image: "nowhere.exe".into(),
            wait: true,
            ..Default::default()
        })
        .expect_err("a name nothing serves cannot launch either");
    assert!(
        err.contains("nowhere.exe") && err.contains("nothing to stage"),
        "a path nothing serves is a different failure from an unstageable one: {err}"
    );

    // And the default is no longer a plausible-looking game exe that would
    // have taken this same doomed path without anyone naming it.
    let err = session
        .launch(&LaunchOpts::default())
        .expect_err("a default-constructed LaunchOpts names no image");
    assert!(err.contains("image is empty"), "{err}");

    session.stop_serve();
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
