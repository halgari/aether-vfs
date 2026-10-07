//! Content under a managed root comes from the director, byte for byte, over both ring
//! transports, and never from the real file under the root.
//!
//! These were the copy-up tests (`cow_seed_reads_through_director.rs`): the shim-local overlay
//! materialised a file's existing content through the director before a preserving write, and
//! each test drove that loop through the removed shim-local `Engine::decide_open` directly. Task C8 removed the overlay,
//! and with it the shim's own copy loop: a preserving write is now the director's (`OPEN_WRITE`
//! without truncation, copy-up inside the provider graph). What the tests were really about —
//! the client's fragmented read over awkward answers, and where the bytes come from — is the
//! same code a read hook uses (`NtReadFile` → `FuseClient::read_fragmented`), so each claim is
//! now made on a read through the hooks:
//!
//! - the provider's bytes, never the real file's (`copy_up_takes_the_provider_graph_s_bytes`,
//!   `copy_up_never_seeds_from_a_real_file_under_the_root`);
//! - a large file over the bulk arena, many round trips, byte exact
//!   (`copy_up_of_a_large_file_spans_round_trips_and_is_byte_exact`);
//! - a sub-threshold file fragmenting over the inline transport
//!   (`a_sub_threshold_copy_up_fragments_over_the_inline_transport`);
//! - a short read, inline and bulk, is not the end of the file
//!   (`a_short_read_is_resumed_rather_than_taken_for_the_end_of_the_file`,
//!   `a_short_bulk_read_is_resumed_too`);
//! - a director error fails the read, with no fallback to the real file
//!   (`a_director_error_fails_the_copy_up_and_leaves_nothing_behind`);
//! - every director handle opened is closed, failed reads included
//!   (`copy_up_closes_the_handles_it_opens_including_failed_reads`);
//! - a named stream is its own content, not the file's
//!   (`a_write_to_an_alternate_data_stream_seeds_nothing`).
//!
//! Three more pin what the conversion could otherwise hide: a read vpath crosses the ring
//! folded (the fake folds every name, so nothing else would notice the shim stop folding), a
//! preserving write to a file only on real disk never seeds from it, and a director file shorter
//! than its OPEN size reads silently truncated (a known issue, pinned).
//!
//! Each test runs in its own process (`isolate!`) and installs its own fake director.

use crate::fakedirector;

use fakedirector::{ARENA_LEN, Fake, PAYLOAD_CAP, ReadStyle, pattern};
use std::io::Write;
use vfs_shim::{HookGuard, install};

/// Bytes only the director has.
const PROVIDER: &[u8] = b"the provider graph's bytes, which only the director can hand over";
/// Bytes only the real filesystem under the root has. Nothing may read these.
const ON_DISK: &[u8] = b"a real file physically on disk under the managed root";

/// A managed root with `fake` behind it and the hooks installed. `arena_len` as for
/// [`fakedirector::install`].
fn session(
    name: &str,
    fake: Fake,
    arena_len: usize,
) -> (std::path::PathBuf, &'static Fake, HookGuard) {
    let base =
        std::env::temp_dir().join(format!("vfs-director-reads-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(root.join("Data")).unwrap();
    let fake = fakedirector::install(&root, fake, arena_len);
    let hooks = install().expect("install");
    (root, fake, hooks)
}

/// The provider's bytes, never the real file's: for a read, and for a preserving write, whose
/// existing content is the provider's too. The real file under the root holds different bytes
/// at the same path, so either mistake is visible.
#[test]
fn a_read_and_a_preserving_write_take_the_provider_graph_s_bytes() {
    isolate!();
    let (root, fake, hooks) = session(
        "provider",
        Fake::new()
            .with("data/plugin.esp", PROVIDER.to_vec(), ReadStyle::Whole)
            .writable_under("data/"),
        0,
    );
    let real = root.join("Data").join("plugin.esp");
    // Written with the detours live, so through the shim's own I/O bypass: a file on real disk
    // that the VFS must ignore.
    vfs_shim::as_shim_io_for_tests(|| std::fs::write(&real, ON_DISK)).unwrap();

    let read = std::fs::read(&real);
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&real)
            .expect("a preserving write to a served file must open");
        f.write_all(b"+").unwrap();
    }
    let after = std::fs::read(&real);
    drop(hooks);

    assert_eq!(
        read.ok().as_deref(),
        Some(PROVIDER),
        "the read did not come from the director"
    );
    let mut want = PROVIDER.to_vec();
    want.push(b'+');
    assert_eq!(
        after.ok().as_deref(),
        Some(want.as_slice()),
        "a preserving write must keep the provider's bytes"
    );
    assert_eq!(fake.contents("data/plugin.esp"), Some(want));
    assert_eq!(
        std::fs::read(&real).ok().as_deref(),
        Some(ON_DISK),
        "the real file under the root was read from or written to"
    );
}

/// A large file over the bulk arena, the transport production uses for anything sizeable: many
/// round trips, and every byte in place.
///
/// The `bulk_reads` assertion is the load-bearing one for coverage: without it this test would
/// pass on the inline path if the arena ever stopped being configured.
#[test]
fn a_large_read_spans_round_trips_over_the_arena_and_is_byte_exact() {
    isolate!();
    let want = pattern(700 * 1024);
    let (root, fake, hooks) = session(
        "large",
        Fake::new().with("data/big.bin", want.clone(), ReadStyle::Whole),
        ARENA_LEN,
    );
    let got = std::fs::read(root.join("Data").join("big.bin")).unwrap_or_default();
    drop(hooks);

    assert_eq!(got.len(), want.len(), "the read stopped short");
    assert!(got == want, "the right length but the wrong bytes");
    let bulk = fake.tally.bulk_reads("data/big.bin");
    assert!(
        bulk >= 3,
        "only {bulk} READs went through the shared arena; a 700 KiB file in 256 KiB banks needs \
         at least 3, and 0 means this covered the inline transport only"
    );
}

/// The inline transport's own fragmentation, counted on the server side. 63 KiB stays below
/// `BULK_THRESHOLD` even with no arena, and a 4088-byte inline response cannot carry it in fewer
/// than 15 READs.
#[test]
fn a_sub_threshold_read_fragments_over_the_inline_transport() {
    isolate!();
    let want = pattern(63 * 1024);
    let (root, fake, hooks) = session(
        "inline",
        Fake::new().with("data/inline.bin", want.clone(), ReadStyle::Whole),
        0,
    );
    let got = std::fs::read(root.join("Data").join("inline.bin")).unwrap_or_default();
    drop(hooks);

    let reads = fake.tally.reads("data/inline.bin");
    let minimum = (want.len() / (PAYLOAD_CAP as usize - 8)) as u64;
    assert!(
        reads >= minimum,
        "a {}-byte file over a {PAYLOAD_CAP}-byte payload cap took {reads} READs; at least \
         {minimum} are structurally required",
        want.len()
    );
    assert_eq!(
        fake.tally.bulk_reads("data/inline.bin"),
        0,
        "no arena, so nothing goes bulk"
    );
    assert!(got == want, "bytes differ across the fragment boundaries");
}

/// A short read is not the end of the file. The provider hands back at most 7 bytes per READ
/// inline, and 100 KiB per READ over the arena, however much is asked for: what a provider does
/// whenever its own backing read comes back partial.
///
/// Both transports, because they return their length through different code
/// (`decode_read_bulk_resp` and an arena copy, against `decode_read_resp_into`).
#[test]
fn a_short_read_is_not_taken_for_the_end_of_the_file_on_either_transport() {
    isolate!();
    let inline = pattern(5_000);
    let bulk = pattern(500 * 1024);
    let (root, fake, hooks) = session(
        "short",
        Fake::new()
            .with("data/dribble.bin", inline.clone(), ReadStyle::Short(7))
            .with(
                "data/bulk-dribble.bin",
                bulk.clone(),
                ReadStyle::Short(100_000),
            ),
        ARENA_LEN,
    );
    let got_inline = std::fs::read(root.join("Data").join("dribble.bin")).unwrap_or_default();
    let got_bulk = std::fs::read(root.join("Data").join("bulk-dribble.bin")).unwrap_or_default();
    drop(hooks);

    assert_eq!(
        got_inline.len(),
        inline.len(),
        "an inline short read was treated as EOF"
    );
    assert!(got_inline == inline, "resumed at the wrong offset");
    assert_eq!(
        got_bulk.len(),
        bulk.len(),
        "a bulk short read was treated as EOF"
    );
    assert!(
        got_bulk == bulk,
        "resumed at the wrong offset, or read a stale arena bank"
    );
    assert!(
        fake.tally.bulk_reads("data/bulk-dribble.bin") >= 5,
        "the bulk fixture did not take the bulk path at all"
    );
}

/// A director error fails the read. There is no fallback to the real file under the root, which
/// sits right there with other bytes: falling back at the moment something went wrong would be
/// the escape.
#[test]
fn a_director_read_error_fails_the_read_and_never_falls_back_to_the_real_file() {
    isolate!();
    let (root, _fake, hooks) = session(
        "error",
        Fake::new().with("data/broken.bin", pattern(50_000), ReadStyle::Error),
        0,
    );
    let real = root.join("Data").join("broken.bin");
    vfs_shim::as_shim_io_for_tests(|| std::fs::write(&real, ON_DISK)).unwrap();
    let got = std::fs::read(&real);
    drop(hooks);

    assert!(
        got.is_err(),
        "a read the director failed must fail, got {:?}",
        got.as_deref().map(String::from_utf8_lossy)
    );
}

/// Every director handle a read opens is closed, the failing read included. A leaked `fh` is a
/// provider-side file the director never releases.
#[test]
fn every_director_handle_a_read_opens_is_closed_failed_reads_included() {
    isolate!();
    let (root, fake, hooks) = session(
        "close",
        Fake::new()
            .with("data/closecheck.esp", PROVIDER.to_vec(), ReadStyle::Whole)
            .with(
                "data/closecheck-broken.bin",
                pattern(9_000),
                ReadStyle::Error,
            ),
        0,
    );
    let _ = std::fs::read(root.join("Data").join("closecheck.esp"));
    let _ = std::fs::read(root.join("Data").join("closecheck-broken.bin"));
    drop(hooks);

    for vpath in ["data/closecheck.esp", "data/closecheck-broken.bin"] {
        assert!(
            fake.tally.opens(vpath) >= 1,
            "{vpath}: never opened through the director"
        );
        assert_eq!(
            fake.tally.closes(vpath),
            fake.tally.opens(vpath),
            "{vpath}: a director handle was not closed"
        );
    }
}

/// A write to a **named alternate data stream** is the stream's, not the file's. Answering
/// `f.esp:probe` with `f.esp`'s bytes, or writing `f.esp` on its behalf, is a containment bug
/// this project has had once on the read path (`vfs-fixture-escape`'s vector 11, which is why
/// `FuseClient::vpath_under_root` carries the suffix).
#[test]
fn a_write_to_an_alternate_data_stream_leaves_the_file_s_own_bytes_alone() {
    isolate!();
    let (root, fake, hooks) = session(
        "stream",
        Fake::new()
            .with("data/streamed.esp", PROVIDER.to_vec(), ReadStyle::Whole)
            .writable_under("data/"),
        0,
    );
    let stream = format!("{}:probe", root.join("Data").join("streamed.esp").display());
    let written = std::fs::write(&stream, b"stream bytes");
    let base = std::fs::read(root.join("Data").join("streamed.esp"));
    drop(hooks);

    written.expect("a write to a named stream under a writable mount must succeed");
    assert_eq!(
        fake.contents("data/streamed.esp:probe").as_deref(),
        Some(&b"stream bytes"[..]),
        "the stream's bytes must land under the stream's own vpath"
    );
    assert_eq!(
        fake.contents("data/streamed.esp").as_deref(),
        Some(PROVIDER),
        "a write to `streamed.esp:probe` changed `streamed.esp`"
    );
    assert_eq!(
        base.ok().as_deref(),
        Some(PROVIDER),
        "the base file reads as itself"
    );
    assert_eq!(
        fake.tally.writes("data/streamed.esp"),
        0,
        "the base file was written on behalf of a stream write"
    );
}

/// A read open crosses the ring with the folded vpath. The fake director folds every name it is
/// sent, so without this a shim that stopped folding would pass every other test here, and a
/// real provider graph keyed by folded names would then miss the file.
#[test]
fn a_read_open_puts_the_folded_vpath_on_the_wire() {
    isolate!();
    let (root, fake, hooks) = session(
        "fold",
        Fake::new().with("data/mixed.esp", PROVIDER.to_vec(), ReadStyle::Whole),
        0,
    );
    let got = std::fs::read(root.join("Data").join("MiXeD.ESP"));
    drop(hooks);

    assert_eq!(got.ok().as_deref(), Some(PROVIDER));
    let reads: Vec<String> = fake
        .tally
        .wire_opens()
        .into_iter()
        .filter(|(flags, _)| flags & vfs_protocol::OPEN_WRITE == 0)
        .map(|(_, v)| v)
        .collect();
    assert!(
        reads.iter().any(|v| v == "data/mixed.esp"),
        "the read open did not send the folded vpath: {reads:?}"
    );
    assert!(
        reads.iter().all(|v| v == &v.to_lowercase()),
        "a read open sent an unfolded vpath: {reads:?}"
    );
}

/// A preserving write (`OPEN_ALWAYS`, append) to a file that exists only on real disk under the
/// root, inside a writable mount: the director creates it, and its prior content is nothing —
/// never the real file's bytes. The real file is untouched.
#[test]
fn a_preserving_write_to_a_file_only_on_real_disk_never_seeds_from_it() {
    isolate!();
    let (root, fake, hooks) = session("seed", Fake::new().writable_under("data/"), 0);
    let real = root.join("Data").join("disk-only.esp");
    vfs_shim::as_shim_io_for_tests(|| std::fs::write(&real, ON_DISK)).unwrap();
    let written = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&real)
        .and_then(|mut f| f.write_all(b"+"));
    drop(hooks);

    written.expect("a preserving create under a writable mount must succeed");
    assert_eq!(
        fake.contents("data/disk-only.esp").as_deref(),
        Some(&b"+"[..]),
        "the director's copy must hold only what was written: no bytes from the real file"
    );
    assert_eq!(
        std::fs::read(&real).ok().as_deref(),
        Some(ON_DISK),
        "the real file under the root was written to"
    );
}

/// **Known issue, pinned.** The director serves 9 000 bytes of a file its `OP_OPEN` and
/// `OP_GETATTR` call 50 000 bytes long. The read hook answers the read at offset 9 000 with
/// `STATUS_SUCCESS` and 0 bytes, which every caller takes for the end of the file, so the read
/// succeeds, silently truncated. (Copy-up, removed by task C8, failed this case instead.) This
/// test records the behaviour so a change to it is noticed; when the read hook is made to fail a
/// short-of-size read, flip it.
#[test]
fn a_file_shorter_than_its_open_size_reads_silently_truncated_known_issue() {
    isolate!();
    let (root, _fake, hooks) = session(
        "liar",
        Fake::new().with(
            "data/liar.bin",
            pattern(50_000),
            ReadStyle::ShorterThanClaimed(9_000),
        ),
        0,
    );
    let got = std::fs::read(root.join("Data").join("liar.bin"));
    drop(hooks);

    let got = got.expect("today the read does not fail");
    assert_eq!(
        got.len(),
        9_000,
        "today the read stops where the director's data stops"
    );
    assert!(
        got == pattern(50_000)[..9_000],
        "and what it does return is the right prefix"
    );
}
