//! Copy-up sources its bytes from the director (gate 4, task 4).
//!
//! Its own test binary because it installs a process-global `FuseClient`
//! (`fuse_client::try_init_from_env`) and sets the environment that names the
//! ring — the convention this workspace states at `VA_LOCK`: a test asserting
//! on process-global state either takes the lock or lives alone. Every test
//! here shares one fake director (see `fakedirector`) and one root, and none
//! of them mutate that global after the first `install`.
//!
//! What the fake buys is the one distinction no filesystem fixture can make:
//! bytes that exist **only** in the provider graph, and bytes that exist
//! **only** on real disk under the managed root. Copy-up must take the first
//! and must never take the second.

mod fakedirector;

use fakedirector::{pattern, Fake, ReadStyle, PAYLOAD_CAP};
use std::sync::OnceLock;
use vfs_redirect::{RootId, FILE_OPEN_IF};
use vfs_shim::{overlay_layer_dir, Engine};

/// GENERIC_WRITE — `classify_open` reads this as a write intent, and
/// `FILE_OPEN_IF` is the disposition that preserves existing content, which
/// together are what ask for a copy-up.
const WRITE: u32 = 0x4000_0000;

/// Bytes only the director has.
const PROVIDER: &[u8] = b"the provider graph's bytes, which only the director can hand over";
/// Bytes only the real filesystem under the root has. Nothing may read these.
const ON_DISK: &[u8] = b"a real file physically on disk under the managed root";
/// Bytes the *snapshot* publishes for the same vpath as `PROVIDER`, on a real
/// file outside the root. `cow_seed` used to copy exactly this (the
/// `Decision::Redirect` arm), so it is the discriminator that says the
/// snapshot is no longer consulted for copy-up content.
const SNAPSHOT_DECOY: &[u8] = b"the snapshot's backing file, which copy-up must no longer read";

struct Fixture {
    root: std::path::PathBuf,
    overlay: std::path::PathBuf,
    engine: Engine,
    fake: &'static Fake,
}

impl Fixture {
    /// The overlay file a copy-up for `rel` would land in.
    fn dest(&self, rel: &[&str]) -> std::path::PathBuf {
        let mut p = overlay_layer_dir(&self.overlay, RootId::DEFAULT);
        for c in rel {
            p = p.join(c);
        }
        p
    }

    /// The NT path a hooked open would present for `rel` under the root.
    fn nt(&self, rel: &[&str]) -> String {
        let mut p = self.root.clone();
        for c in rel {
            p = p.join(c);
        }
        format!(r"\??\{}", p.to_string_lossy())
    }
}

/// The one root, one overlay, one director every test here shares.
fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let base = std::env::temp_dir().join(format!("vfs-cowseed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("root");
        let overlay = base.join("overlay");
        let decoy_dir = base.join("snapshot-backing");
        std::fs::create_dir_all(root.join("Data")).unwrap();
        std::fs::create_dir_all(&overlay).unwrap();
        std::fs::create_dir_all(&decoy_dir).unwrap();

        // The negative canary, in miniature: a real file, physically under the
        // managed root, that the director does not serve.
        std::fs::write(root.join("Data").join("only-on-disk.bin"), ON_DISK).unwrap();
        // The snapshot's backing file for `data/only-in-graph.esp`.
        let decoy = decoy_dir.join("only-in-graph.esp");
        std::fs::write(&decoy, SNAPSHOT_DECOY).unwrap();

        let snapshot = {
            use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
            let tree = build(vec![Layer {
                id: LayerId(0),
                entries: vec![InputEntry {
                    vpath: "data/only-in-graph.esp".into(),
                    kind: EntryKind::File,
                    source: decoy.to_str().unwrap().into(),
                    size: SNAPSHOT_DECOY.len() as u64,
                    mtime: 1,
                }],
            }])
            .unwrap();
            vfs_shared::bridge::flatten(&tree)
        };

        let fake = fakedirector::install(
            &root,
            Fake::new()
                .with("data/only-in-graph.esp", PROVIDER.to_vec(), ReadStyle::Whole)
                .with("data/to-rename.esp", PROVIDER.to_vec(), ReadStyle::Whole)
                .with("data/big.bin", pattern(700 * 1024), ReadStyle::Whole)
                .with("data/dribble.bin", pattern(5_000), ReadStyle::Short(7))
                .with(
                    "data/bulk-dribble.bin",
                    pattern(500 * 1024),
                    ReadStyle::Short(100_000),
                )
                .with("data/broken.bin", pattern(50_000), ReadStyle::Error)
                .with(
                    "data/liar.bin",
                    pattern(50_000),
                    ReadStyle::ShorterThanClaimed(9_000),
                )
                .with("data/closecheck.esp", PROVIDER.to_vec(), ReadStyle::Whole)
                .with("data/closecheck-broken.bin", pattern(9_000), ReadStyle::Error)
                // Just under `BULK_THRESHOLD`, so it stays inline whatever the
                // arena is: the inline transport's own fragmentation case.
                .with("data/inline-roundtrips.bin", pattern(63 * 1024), ReadStyle::Whole)
                .with("data/streamed.esp", PROVIDER.to_vec(), ReadStyle::Whole),
            // A real bulk arena. Copy-up's 256 KiB `SEED_CHUNK` is four times
            // `BULK_THRESHOLD`, so live, every large-file copy-up rides the
            // arena; a fixture without one tests fragment reassembly only on
            // the transport production does not take.
            fakedirector::ARENA_LEN,
        );

        let engine = Engine::with_overlay(
            root.to_str().unwrap(),
            overlay.to_str().unwrap(),
            snapshot,
        )
        .unwrap();
        Fixture { root, overlay, engine, fake }
    })
}

/// **The positive half.** A file present only in the provider graph — no such
/// file exists on real disk under the root — is opened with a preserving
/// disposition, and the copy-up destination receives the *provider's* bytes.
///
/// The snapshot publishes the same vpath, backed by a real file with different
/// content, which is what the old `cow_seed` copied. So this fails two
/// distinct ways against the old implementation (nothing at all if the
/// snapshot is dropped, the decoy's bytes if it is kept) and only one way
/// against the new one.
#[test]
fn copy_up_takes_the_provider_graph_s_bytes() {
    let f = fixture();
    let rel = ["Data", "only-in-graph.esp"];
    let dest = f.dest(&["data", "only-in-graph.esp"]);
    assert!(
        !f.root.join("Data").join("only-in-graph.esp").exists(),
        "setup: this vpath must exist ONLY in the director, never on disk under the root"
    );

    f.engine.decide_open(&f.nt(&rel), WRITE, FILE_OPEN_IF);

    assert_eq!(
        std::fs::read(&dest).unwrap_or_default(),
        PROVIDER,
        "copy-up must materialise the director's bytes at {dest:?}"
    );
    assert_ne!(
        std::fs::read(&dest).unwrap_or_default(),
        SNAPSHOT_DECOY,
        "copy-up read the snapshot's backing file instead of asking the director"
    );
}

/// **The invariant.** A file present only on real disk under the managed root
/// — the negative canary — must not be seeded from. This is the assertion the
/// whole task exists for: reads have been sealed since gate 3, and
/// `vfs-directord`'s escape matrix scopes its negative-canary check to reads
/// precisely because a *write* still reached the file through copy-up.
#[test]
fn copy_up_never_seeds_from_a_real_file_under_the_root() {
    let f = fixture();
    let rel = ["Data", "only-on-disk.bin"];
    let dest = f.dest(&["data", "only-on-disk.bin"]);
    assert_eq!(
        std::fs::read(f.root.join("Data").join("only-on-disk.bin")).unwrap(),
        ON_DISK,
        "setup: the real file must genuinely be there for this to mean anything"
    );

    let d = f.engine.decide_open(&f.nt(&rel), WRITE, FILE_OPEN_IF);

    // The write is still captured — sealing the seed must not push the write
    // itself back onto real disk, which would be a worse failure.
    assert!(
        matches!(d, vfs_redirect::Decision::Redirect { .. }),
        "the write must still be redirected into the overlay, got {d:?}"
    );
    assert_ne!(
        std::fs::read(&dest).unwrap_or_default(),
        ON_DISK,
        "copy-up seeded {dest:?} from the real file under the managed root — the exact \
         content the invariant says is unreachable by any spelling"
    );
    assert!(
        !dest.exists(),
        "the director does not serve this path, so nothing should have been created at \
         {dest:?} at all; the caller's own write creates it empty"
    );
}

/// A file far larger than one round trip can carry, **over the transport a
/// real copy-up uses**. `SEED_CHUNK` is 256 KiB, four times `BULK_THRESHOLD`,
/// so each read is answered out of the shared arena: the ring carries only a
/// length and an offset, and the bytes are copied from an arena bank that is
/// reused across requests. An implementation that reads once produces 256 KiB;
/// one that mishandles bank offsets or lets a bank be reused before it is
/// copied produces 700 KiB of the wrong bytes.
///
/// The `bulk_reads` assertion is the load-bearing one for coverage: without it
/// this test would silently pass on the inline path if the arena ever stopped
/// being configured, which is precisely the gap it was written to close.
#[test]
fn copy_up_of_a_large_file_spans_round_trips_and_is_byte_exact() {
    let f = fixture();
    let want = pattern(700 * 1024);
    let rel = ["Data", "big.bin"];
    let dest = f.dest(&["data", "big.bin"]);

    f.engine.decide_open(&f.nt(&rel), WRITE, FILE_OPEN_IF);

    let got = std::fs::read(&dest).unwrap_or_default();
    assert_eq!(
        got.len(),
        want.len(),
        "copy-up stopped short: {} of {} bytes",
        got.len(),
        want.len()
    );
    assert!(got == want, "copy-up produced the right length but the wrong bytes");
    let bulk = f.fake.tally.bulk_reads("data/big.bin");
    assert!(
        bulk >= 3,
        "only {bulk} of this copy-up's reads went through the shared arena; a 700 KiB \
         file at a 256 KiB seed chunk needs at least 3, and if this is 0 the test just \
         covered the inline transport that production does not use for large files"
    );
}

/// A short read on the **bulk** path is resumed too. Same claim as the inline
/// short-read test, on the other transport: the provider serves 100 KiB per
/// READ however much the 256 KiB seed chunk asked for, which is not EOF.
///
/// Worth having separately because the two transports return their length
/// through different code (`decode_read_bulk_resp` + an arena copy, versus
/// `decode_read_resp_into`), and a short bulk read additionally means the
/// bank holds less than the client asked for.
#[test]
fn a_short_bulk_read_is_resumed_too() {
    let f = fixture();
    let want = pattern(500 * 1024);
    let dest = f.dest(&["data", "bulk-dribble.bin"]);

    f.engine.decide_open(&f.nt(&["Data", "bulk-dribble.bin"]), WRITE, FILE_OPEN_IF);

    let got = std::fs::read(&dest).unwrap_or_default();
    assert_eq!(got.len(), want.len(), "a short bulk read was treated as EOF");
    assert!(got == want, "resumed at the wrong offset, or read a stale arena bank");
    assert!(
        f.fake.tally.bulk_reads("data/bulk-dribble.bin") >= 5,
        "this fixture did not take the bulk path at all"
    );
}

/// A short read is not EOF. The fake hands back at most 7 bytes per READ
/// however much is asked for, which is what a provider does whenever its own
/// backing read comes back partial. A reader that takes the first short answer
/// as the end of the file writes 7 bytes and calls it a copy.
#[test]
fn a_short_read_is_resumed_rather_than_taken_for_the_end_of_the_file() {
    let f = fixture();
    let want = pattern(5_000);
    let rel = ["Data", "dribble.bin"];
    let dest = f.dest(&["data", "dribble.bin"]);

    f.engine.decide_open(&f.nt(&rel), WRITE, FILE_OPEN_IF);

    let got = std::fs::read(&dest).unwrap_or_default();
    assert_eq!(got.len(), want.len(), "a short read was treated as EOF");
    assert!(got == want, "resumed at the wrong offset");
}

/// A director error fails the copy-up outright — no `std::fs::copy` fallback,
/// which is the escape this task removes, and no half-written file left behind
/// for the game to edit believing it whole.
///
/// Two shapes of failure, because they fail in different places: a READ that
/// returns an error status, and a file that turns out shorter than the OPEN
/// response claimed.
#[test]
fn a_director_error_fails_the_copy_up_and_leaves_nothing_behind() {
    let f = fixture();
    for name in ["broken.bin", "liar.bin"] {
        let dest = f.dest(&["data", name]);
        f.engine.decide_open(&f.nt(&["Data", name]), WRITE, FILE_OPEN_IF);
        assert!(
            !dest.exists(),
            "{name}: a failed copy-up must leave no partial file at {dest:?} — a truncated \
             seed is worse than an empty one, because the game edits it believing it whole"
        );
    }
}

/// The second call site. `Engine::rename` materialises the source before
/// moving it, and it must materialise it the same way `decide_open` does.
#[test]
fn rename_materialises_its_source_through_the_director_too() {
    let f = fixture();
    let from = f.nt(&["Data", "to-rename.esp"]);
    let to = f.nt(&["Data", "renamed.esp"]);
    let dest = f.dest(&["data", "renamed.esp"]);

    assert_eq!(
        f.engine.rename(&from, &to),
        vfs_shim::RenameOutcome::Handled,
        "the rename must be handled by the overlay"
    );

    assert_eq!(
        std::fs::read(&dest).unwrap_or_default(),
        PROVIDER,
        "rename's copy-up must read the director too, not the filesystem"
    );
}

/// Every OPEN copy-up makes is closed, including the ones that fail part-way.
/// A director handle leaked per copy-up is a provider-side file nothing
/// releases for the life of the session, and the failing paths are where a
/// hand-written close is easiest to miss.
///
/// Uses vpaths no other test touches, so the counts are this test's alone
/// however the harness interleaves.
#[test]
fn copy_up_closes_the_handles_it_opens_including_failed_reads() {
    let f = fixture();
    // A clean copy-up and a copy-up whose reads all fail.
    for name in ["closecheck.esp", "closecheck-broken.bin"] {
        let vpath = format!("data/{}", name.to_ascii_lowercase());
        f.engine.decide_open(&f.nt(&["Data", name]), WRITE, FILE_OPEN_IF);
        assert_eq!(
            f.fake.tally.opens(&vpath),
            1,
            "{name}: copy-up must have opened it through the director exactly once"
        );
        assert_eq!(
            f.fake.tally.closes(&vpath),
            f.fake.tally.opens(&vpath),
            "{name}: the director handle was not closed"
        );
    }
}

/// The inline transport's own fragmentation, counted on the server side rather
/// than inferred. This fixture is 63 KiB — deliberately just under
/// `BULK_THRESHOLD`, so it stays inline even though the ring has an arena —
/// and a 4088-byte inline response cannot carry it in fewer than 15 READs.
#[test]
fn a_sub_threshold_copy_up_fragments_over_the_inline_transport() {
    let f = fixture();
    let want = pattern(63 * 1024);
    f.engine.decide_open(&f.nt(&["Data", "inline-roundtrips.bin"]), WRITE, FILE_OPEN_IF);
    let vpath = "data/inline-roundtrips.bin";
    let reads = f.fake.tally.reads(vpath);
    let minimum = (want.len() / (PAYLOAD_CAP as usize - 8)) as u64;
    assert!(
        reads >= minimum,
        "a {}-byte file over a {PAYLOAD_CAP}-byte payload cap took {reads} READs; at least \
         {minimum} are structurally required, so this copy-up did not fragment",
        want.len()
    );
    assert_eq!(
        f.fake.tally.bulk_reads(vpath),
        0,
        "a request below BULK_THRESHOLD must stay inline; this test covers nothing \
         the large-file test does not if it went bulk"
    );
    assert!(
        std::fs::read(f.dest(&["data", vpath.rsplit('/').next().unwrap()])).unwrap_or_default()
            == want,
        "bytes differ across the fragment boundaries"
    );
}

/// A preserving write to a **named alternate data stream** seeds nothing.
///
/// `Engine::resolve` goes through `RootMap::canonicalise`, which drops the
/// `:stream` suffix — right for unifying spellings of a file, wrong as the
/// vpath to copy up from, because the base file's content is not the stream's.
/// Answering `f.esp:probe` with `f.esp`'s bytes is a containment bug this
/// project has already had once on the read path (`vfs-fixture-escape`'s
/// vector 11, which is why `FuseClient::vpath_under_root` re-attaches the
/// suffix). Copy-up declining is the honest answer; inventing stream support
/// is a different task.
///
/// The base file is in the provider graph and copies up fine by its own name,
/// which is what makes this test discriminating rather than an assertion that
/// nothing works.
#[test]
fn a_write_to_an_alternate_data_stream_seeds_nothing() {
    let f = fixture();
    let base = f.dest(&["data", "streamed.esp"]);
    let stream_nt = format!("{}:probe", f.nt(&["Data", "streamed.esp"]));

    let d = f.engine.decide_open(&stream_nt, WRITE, FILE_OPEN_IF);

    // Still redirected into the overlay: declining to *seed* must not push the
    // write itself back to real disk.
    assert!(
        matches!(d, vfs_redirect::Decision::Redirect { .. }),
        "the stream write must still be captured by the overlay, got {d:?}"
    );
    assert!(
        !base.exists(),
        "a write to `streamed.esp:probe` copied up `streamed.esp`'s content to {base:?} — \
         the base file's bytes are not the named stream's"
    );
    assert_eq!(
        f.fake.tally.opens("data/streamed.esp"),
        0,
        "the director was asked for the base file on behalf of a stream write"
    );

    // Control: the same file, named without the suffix, does copy up. Without
    // this the assertions above would also pass if copy-up were simply broken.
    f.engine.decide_open(&f.nt(&["Data", "streamed.esp"]), WRITE, FILE_OPEN_IF);
    assert_eq!(
        std::fs::read(&base).unwrap_or_default(),
        PROVIDER,
        "the base file must still copy up under its own name"
    );
}
