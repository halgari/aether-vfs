#[path = "../../aether-net/tests/common/mod.rs"]
mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use aether_net::{Events, SourceError};
use aether_nexus::{NexusArchive, NexusClient};
use common::repack::{RepackFile, data, repacked_zip};
use common::{API_KEY, Canned, TestServer, http, start};

const UID: u64 = (1704u64 << 32) + 75329;

struct Fixture {
    server: TestServer,
    client: Arc<NexusClient>,
    esp: Vec<u8>,
    big: Vec<u8>,
}

/// A repacked zip with: a small single-frame entry, directory entries, and
/// a 300 KiB entry split into 64 KiB frames with a 0x4E58 seek table.
async fn fixture() -> Fixture {
    let server = start().await;
    let esp = data(17_710, 1);
    let big = data(300 << 10, 2);
    server.put_repacked(
        UID,
        repacked_zip(&[
            RepackFile::new("LovelyLetter.esp", esp.clone()),
            RepackFile::dir("Source/"),
            RepackFile::dir("Source/Scripts/"),
            RepackFile::new("Textures/Big.dds", big.clone()).frames(64 << 10),
        ]),
    );
    let client = Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&server.base)
            .unwrap(),
    );
    Fixture {
        server,
        client,
        esp,
        big,
    }
}

fn file_gets(s: &TestServer) -> Vec<String> {
    s.requests("/repacked/")
        .into_iter()
        .map(|l| l.range.unwrap_or_default())
        .collect()
}

/// What an open asks the file host for first: the zip's last 256 KiB.
const TAIL: &str = "bytes=-262144";

#[tokio::test]
async fn open_is_one_request_with_no_size_probe() {
    let f = fixture().await;
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    assert_eq!(a.index().entries().len(), 4);
    assert_eq!(a.find(r"textures\BIG.DDS"), Some(3));
    assert_eq!(a.find("missing.esp"), None);
    // The tail alone: its Content-Range gives the length, and the
    // directory is inside it.
    let gets = file_gets(&f.server);
    assert_eq!(gets, [TAIL]);
    assert_eq!(
        f.server.requests("/v3/").len(),
        1,
        "one signed URL, then cached"
    );
}

/// A repack of one small file: far shorter than the tail an open asks for.
async fn tiny() -> (TestServer, Arc<NexusClient>, Vec<u8>, u64) {
    let server = start().await;
    let esp = data(700, 9);
    let zip = repacked_zip(&[RepackFile::new("Tiny.esp", esp.clone())]);
    let len = zip.len() as u64;
    assert!(len < 4096, "{len}");
    server.put_repacked(UID, zip);
    let client = Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&server.base)
            .unwrap(),
    );
    (server, client, esp, len)
}

#[tokio::test]
async fn an_archive_smaller_than_the_tail_opens_in_one_request() {
    let (server, client, esp, len) = tiny().await;
    let a = NexusArchive::open(client, UID).await.unwrap();
    assert_eq!(a.len(), len);
    assert_eq!(file_gets(&server), [TAIL]);
    assert_eq!(
        a.read_entry(a.find("tiny.esp").unwrap()).await.unwrap(),
        esp
    );
}

/// The file host may refuse a suffix longer than the file (it answers a
/// range past the end with a 500): the open then asks for the length, as
/// it used to.
#[tokio::test]
async fn a_host_that_refuses_an_overlong_suffix_still_opens_a_small_archive() {
    let (server, client, esp, len) = tiny().await;
    server.strict_suffix(&format!("/repacked/{UID}"));
    let a = NexusArchive::open(client, UID).await.unwrap();
    assert_eq!(a.len(), len);
    let gets = file_gets(&server);
    assert_eq!(
        gets,
        [
            TAIL.to_string(),
            "bytes=0-0".to_string(),
            format!("bytes=0-{}", len - 1)
        ]
    );
    assert_eq!(server.requests("/v3/").len(), 1, "the link is reused");
    assert_eq!(
        a.read_entry(a.find("tiny.esp").unwrap()).await.unwrap(),
        esp
    );
}

/// A failed suffix request is not tried again as such: the length probe
/// and the tail read that follow are its retry.
#[tokio::test]
async fn a_suffix_request_that_fails_is_followed_by_the_length_probe() {
    let f = fixture().await;
    f.server.script(
        &format!("/repacked/{UID}"),
        Canned {
            status: 503,
            headers: vec![],
            body: b"busy".to_vec(),
        },
    );
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    assert_eq!(a.index().entries().len(), 4);
    let gets = file_gets(&f.server);
    assert_eq!(gets.len(), 3, "{gets:?}");
    assert_eq!(gets[..2], [TAIL, "bytes=0-0"]);
    assert_eq!(f.server.requests("/v3/").len(), 1);
}

/// A host that ignores Range and sends the whole (small) file has also
/// said how long it is.
#[tokio::test]
async fn a_small_archive_sent_whole_in_answer_to_the_suffix_opens() {
    let (server, client, _esp, len) = tiny().await;
    server.no_range(&format!("/repacked/{UID}"));
    let a = NexusArchive::open(client, UID).await.unwrap();
    assert_eq!(a.len(), len);
    assert_eq!(a.index().entries().len(), 1);
    assert_eq!(file_gets(&server), [TAIL]);
}

#[tokio::test]
async fn a_large_archive_on_a_host_without_ranges_is_refused() {
    let f = fixture().await;
    let server = &f.server;
    server.put_repacked(
        UID,
        repacked_zip(&[RepackFile::new("Noise.bin", noise(400 << 10, 3))]),
    );
    server.no_range(&format!("/repacked/{UID}"));
    let e = NexusArchive::open(f.client.clone(), UID).await.unwrap_err();
    assert!(e.to_string().contains("ignored the Range header"), "{e}");
    assert!(!e.to_string().contains("sig="));
}

#[tokio::test]
async fn a_rejected_link_is_renewed_while_opening() {
    let f = fixture().await;
    f.client.download_url(UID).await.unwrap();
    f.server.state.valid_sig.store(2, Ordering::SeqCst); // that URL now 403s
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    assert_eq!(a.index().entries().len(), 4);
    assert_eq!(f.server.requests("/v3/").len(), 2);
    assert_eq!(file_gets(&f.server), [TAIL, TAIL], "refused, then served");
}

#[tokio::test]
async fn read_range_fetches_only_covering_frames_in_one_request() {
    let f = fixture().await;
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    f.server.clear_log();
    let id = a.find("Textures/Big.dds").unwrap();
    let got = a.read_range(id, 70_000..140_000).await.unwrap();
    assert_eq!(got, &f.big[70_000..140_000]);
    // Frames 1 and 2 start near the data's start, so the local header
    // comes in the same request (with frame 0 in between).
    let gets = file_gets(&f.server);
    assert_eq!(gets.len(), 1, "{gets:?}");
    let span = |r: &str| {
        let (a, b) = r.strip_prefix("bytes=").unwrap().split_once('-').unwrap();
        (a.parse::<u64>().unwrap(), b.parse::<u64>().unwrap() + 1)
    };
    let entry = &a.index().entries()[id];
    assert_eq!(span(&gets[0]).0, entry.local_header_offset);
    let t = entry.seek_table.as_ref().unwrap();
    let upto: u64 = t.frames()[..3]
        .iter()
        .map(|f| f.compressed_size as u64)
        .sum();
    let (s, e) = span(&gets[0]);
    assert!(e - s >= upto && e - s < upto + 512, "{gets:?}");

    // The layout is now known: frames 1 and 2 alone are one request.
    f.server.clear_log();
    assert_eq!(
        a.read_range(id, 70_000..140_000).await.unwrap(),
        &f.big[70_000..140_000]
    );
    let gets = file_gets(&f.server);
    let want: u64 = t.frames()[1..3]
        .iter()
        .map(|f| f.compressed_size as u64)
        .sum();
    assert_eq!(gets.len(), 1, "{gets:?}");
    let (s, e) = span(&gets[0]);
    assert_eq!(e - s, want);

    // The layout is cached: a read in frame 0 is one request.
    f.server.clear_log();
    assert_eq!(a.read_range(id, 5..10).await.unwrap(), &f.big[5..10]);
    assert_eq!(file_gets(&f.server).len(), 1);
    // Whole entry, and an empty range (no request).
    assert_eq!(a.read_entry(id).await.unwrap(), f.big);
    f.server.clear_log();
    assert!(a.read_range(id, 7..7).await.unwrap().is_empty());
    assert!(file_gets(&f.server).is_empty());
}

#[tokio::test]
async fn read_entry_uses_one_request_for_a_small_entry() {
    let f = fixture().await;
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    f.server.clear_log();
    let id = a.find("lovelyletter.esp").unwrap();
    assert_eq!(a.read_entry(id).await.unwrap(), f.esp);
    assert_eq!(file_gets(&f.server).len(), 1);
    assert_eq!(a.read_range(id, 100..200).await.unwrap(), &f.esp[100..200]);
    assert!(
        a.read_entry(a.find("Source/").unwrap())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn large_entry_without_extra_field_reads_the_in_stream_table() {
    let server = start().await;
    let big = data(9 << 20, 3);
    let mut f = RepackFile::new("a/huge.dds", big.clone());
    f.nexus_extra = false;
    server.put_repacked(UID, repacked_zip(&[f]));
    let client = Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&server.base)
            .unwrap(),
    );
    let a = NexusArchive::open(client, UID).await.unwrap();
    assert!(a.index().entries()[0].seek_table.is_none());
    let r = (5 << 20) - 3..(5 << 20) + 10;
    assert_eq!(
        a.read_range(0, r.clone()).await.unwrap(),
        &big[r.start as usize..r.end as usize]
    );
}

#[tokio::test]
async fn bad_frame_checksum_is_an_error() {
    let server = start().await;
    let mut f = RepackFile::new("x.dds", data(200 << 10, 4)).frames(64 << 10);
    f.bad_checksum_frame = Some(1);
    server.put_repacked(UID, repacked_zip(&[f]));
    let client = Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&server.base)
            .unwrap(),
    );
    let a = NexusArchive::open(client, UID).await.unwrap();
    assert_eq!(a.read_range(0, 0..100).await.unwrap().len(), 100);
    let err = a.read_range(0, 70_000..70_010).await.unwrap_err();
    assert!(
        matches!(
            err,
            SourceError::Format(aether_archive::FormatError::Checksum { .. })
        ),
        "{err}"
    );
}

#[tokio::test]
async fn rejected_signed_url_is_renewed_once() {
    let f = fixture().await;
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    assert_eq!(f.server.requests("/v3/").len(), 1);
    f.server.state.valid_sig.store(2, Ordering::SeqCst); // old URL now 403s
    let id = a.find("LovelyLetter.esp").unwrap();
    assert_eq!(a.read_entry(id).await.unwrap(), f.esp);
    assert_eq!(f.server.requests("/v3/").len(), 2);
}

#[tokio::test]
async fn urls_close_to_expiry_are_renewed_before_use() {
    let f = fixture().await;
    f.server.state.url_ttl_secs.store(60, Ordering::SeqCst); // inside the 5-minute margin
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    a.read_entry(a.find("LovelyLetter.esp").unwrap())
        .await
        .unwrap();
    assert_eq!(
        f.server.requests("/v3/").len(),
        2,
        "the open and the read each got a fresh URL"
    );
}

#[tokio::test]
async fn errors_never_leak_the_signature() {
    let f = fixture().await;
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    for _ in 0..3 {
        f.server.script(
            &format!("/repacked/{UID}"),
            Canned {
                status: 502,
                headers: vec![],
                body: b"bad gateway".to_vec(),
            },
        );
    }
    let e = a.read_entry(0).await.unwrap_err();
    let msg = e.to_string();
    assert!(msg.contains("502") && !msg.contains("sig="), "{msg}");
    assert!(!format!("{e:?}").contains("sig="));
}

#[tokio::test]
async fn out_of_range_reads_fail_without_a_request() {
    let f = fixture().await;
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    f.server.clear_log();
    let id = a.find("LovelyLetter.esp").unwrap();
    assert!(a.read_range(id, 0..f.esp.len() as u64 + 1).await.is_err());
    assert!(a.read_range(99, 0..1).await.is_err());
    assert!(file_gets(&f.server).is_empty());
}

// --- Fix round 1 regression tests ---

/// Critical #1: a zstd entry with no 0x4E58 extra, a huge claimed
/// uncompressed size (forcing the in-stream-table path) and a tiny claimed
/// compressed size (shorter than the 9-byte seek-table footer) must error,
/// not panic on an underflowing slice.
#[tokio::test]
async fn tiny_entry_claiming_a_large_uncompressed_size_errors_without_panicking() {
    let server = start().await;
    let mut f = RepackFile::new("tiny.dds", data(20, 7));
    f.claimed_uncompressed_size = Some(5_000_000); // > PLAIN_FRAME_MAX
    f.claimed_compressed_size = Some(3); // shorter than the seek-table footer
    server.put_repacked(UID, repacked_zip(&[f]));
    let client = Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&server.base)
            .unwrap(),
    );
    let a = NexusArchive::open(client, UID).await.unwrap();
    let err = a.read_range(0, 0..1).await.unwrap_err();
    assert!(matches!(err, SourceError::Archive { .. }), "{err}");
}

/// Important #2: a local header offset the central directory claims is
/// (far) outside the archive must error instead of overflowing the sums
/// built from it.
#[tokio::test]
async fn local_header_offset_near_u64_max_errors_without_overflow() {
    let server = start().await;
    let mut f = RepackFile::new("x.esp", data(10, 9));
    f.claimed_local_header_offset = Some(u64::MAX - 5);
    server.put_repacked(UID, repacked_zip(&[f]));
    let client = Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&server.base)
            .unwrap(),
    );
    let a = NexusArchive::open(client, UID).await.unwrap();
    let err = a.read_entry(0).await.unwrap_err();
    assert!(matches!(err, SourceError::Archive { .. }), "{err}");
    // read_range shares the same checked helper.
    let err = a.read_range(0, 0..1).await.unwrap_err();
    assert!(matches!(err, SourceError::Archive { .. }), "{err}");
}

/// Important #3a: read_entry must check the in-stream (no 0x4E58) seek
/// table against the directory's uncompressed size, both when the table
/// covers fewer bytes and when it covers more.
#[tokio::test]
async fn read_entry_checks_the_in_stream_table_against_the_directory() {
    for claimed in [(9 << 20) - 1, (9 << 20) + 1] {
        let server = start().await;
        let big = data(9 << 20, 11);
        let mut f = RepackFile::new("a/huge.dds", big);
        f.nexus_extra = false;
        f.claimed_uncompressed_size = Some(claimed);
        server.put_repacked(UID, repacked_zip(&[f]));
        let client = Arc::new(
            NexusClient::new(http(Events::default()), API_KEY)
                .with_base_url(&server.base)
                .unwrap(),
        );
        let a = NexusArchive::open(client, UID).await.unwrap();
        let err = a.read_entry(0).await.unwrap_err();
        assert!(
            matches!(
                err,
                SourceError::Format(aether_archive::FormatError::Invalid { .. })
            ),
            "{claimed}: {err}"
        );
    }
}

/// Important #3b: a stored (method 0) entry whose directory claims
/// different compressed and uncompressed sizes must error in both
/// read_entry and read_range, without fetching anything.
#[tokio::test]
async fn stored_entry_with_mismatched_sizes_is_an_error() {
    let server = start().await;
    let mut f = RepackFile::new("x.bin", data(50, 13));
    f.force_stored = true;
    f.claimed_uncompressed_size = Some(60); // real (and claimed compressed) is 50
    server.put_repacked(UID, repacked_zip(&[f]));
    let client = Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&server.base)
            .unwrap(),
    );
    let a = NexusArchive::open(client, UID).await.unwrap();
    server.clear_log();
    let err = a.read_entry(0).await.unwrap_err();
    assert!(matches!(err, SourceError::Archive { .. }), "{err}");
    let err = a.read_range(0, 0..1).await.unwrap_err();
    assert!(matches!(err, SourceError::Archive { .. }), "{err}");
    assert!(file_gets(&server).is_empty());
}

// --- Final-review fix wave ---

/// Item 4: an in-stream seek table whose footer claims a table larger than
/// 64 MiB is refused before it is fetched.
#[tokio::test]
async fn an_oversized_in_stream_seek_table_is_refused_without_fetching_it() {
    let server = start().await;
    const CLAIMED: u64 = 70 << 20;
    let mut a = RepackFile::new("a.dds", data(20, 21));
    a.claimed_uncompressed_size = Some(5_000_000); // no frame map from the directory
    a.claimed_compressed_size = Some(CLAIMED);
    let mut pad = RepackFile::new("pad.bin", vec![0; (CLAIMED + (1 << 20)) as usize]);
    pad.force_stored = true;
    let mut zip = repacked_zip(&[a, pad]);
    // Entry data starts after the 30-byte header, the name and 56 extra bytes.
    let end = (30 + 5 + 56 + CLAIMED) as usize;
    let frames: u32 = ((65u64 << 20) / 12) as u32;
    zip[end - 9..end - 5].copy_from_slice(&frames.to_le_bytes());
    zip[end - 5] = 0x80;
    zip[end - 4..end].copy_from_slice(&0x8F92_EAB1u32.to_le_bytes());
    server.put_repacked(UID, zip);
    let client = Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&server.base)
            .unwrap(),
    );
    let arc = NexusArchive::open(client, UID).await.unwrap();
    server.clear_log();
    let err = arc.read_range(0, 0..1).await.unwrap_err();
    assert!(matches!(err, SourceError::Archive { .. }), "{err}");
    // Local header and the 64 KiB tail only.
    assert_eq!(file_gets(&server).len(), 2, "{:?}", file_gets(&server));
}

/// Item 8: a renewed signed URL that serves a different-sized archive is an
/// error, not a read from the wrong bytes.
#[tokio::test]
async fn a_renewed_url_serving_a_different_archive_is_an_error() {
    let f = fixture().await;
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    f.server.put_repacked(
        UID,
        repacked_zip(&[RepackFile::new("Other.esp", data(5_000, 5))]),
    );
    f.server.state.valid_sig.store(2, Ordering::SeqCst); // old URL now 403s
    let id = a.find("LovelyLetter.esp").unwrap();
    let err = a.read_entry(id).await.unwrap_err();
    assert!(
        err.to_string().contains("archive changed on the server"),
        "{err}"
    );
    assert_eq!(f.server.requests("/v3/").len(), 2, "renewed once");
}

/// Item 9: an index saved from one open rebuilds the archive without any
/// request; reads then fetch exactly what they would have anyway.
#[tokio::test]
async fn an_archive_rebuilt_from_its_index_reads_the_same_bytes() {
    let f = fixture().await;
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    let (len, index) = (a.len(), a.index().clone());
    f.server.clear_log();
    let b = NexusArchive::from_index(f.client.clone(), UID, len, index).unwrap();
    assert!(f.server.requests("/").is_empty(), "from_index is offline");
    assert_eq!(b.len(), len);
    assert_eq!(b.index().entries().len(), a.index().entries().len());

    let big = b.find("Textures/Big.dds").unwrap();
    let esp = b.find("LovelyLetter.esp").unwrap();
    assert_eq!(
        b.read_range(big, 70_000..140_000).await.unwrap(),
        &f.big[70_000..140_000]
    );
    assert_eq!(b.read_entry(esp).await.unwrap(), f.esp);
    let from_b = file_gets(&f.server);
    // Header with the frames for the range, one request for the whole
    // small entry.
    assert_eq!(from_b.len(), 2, "{from_b:?}");
    assert!(
        f.server.requests("/v3/").is_empty(),
        "signed URL still cached"
    );

    f.server.clear_log();
    assert_eq!(
        a.read_range(big, 70_000..140_000).await.unwrap(),
        &f.big[70_000..140_000]
    );
    assert_eq!(a.read_entry(esp).await.unwrap(), f.esp);
    assert_eq!(
        file_gets(&f.server),
        from_b,
        "same requests as the opened archive"
    );
}

/// Item 10: a central directory larger than the 256 KiB tail read is
/// fetched with a second request, then reads work as usual.
#[tokio::test]
async fn a_central_directory_outside_the_tail_is_fetched_separately() {
    let server = start().await;
    let esp = data(10_000, 17);
    let mut files: Vec<RepackFile> = (0..3000)
        .map(|i| RepackFile::dir(&format!("Meshes/{}/{i:05}/", "x".repeat(80))))
        .collect();
    files.push(RepackFile::new("Last.esp", esp.clone()));
    let zip = repacked_zip(&files);
    let zip_len = zip.len() as u64;
    server.put_repacked(UID, zip);
    let client = Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&server.base)
            .unwrap(),
    );
    let a = NexusArchive::open(client, UID).await.unwrap();
    assert_eq!(a.index().entries().len(), 3001);
    let gets = file_gets(&server);
    assert_eq!(gets.len(), 2, "{gets:?}");
    assert_eq!(gets[0], TAIL);
    let (start, end) = gets[1]
        .strip_prefix("bytes=")
        .unwrap()
        .split_once('-')
        .unwrap();
    let (start, end): (u64, u64) = (start.parse().unwrap(), end.parse().unwrap());
    assert!(start < zip_len - (256 << 10), "{}", gets[1]);
    assert!(
        end + 1 - start > 256 << 10,
        "directory bigger than the tail"
    );
    let id = a.find("last.esp").unwrap();
    assert_eq!(a.read_entry(id).await.unwrap(), esp);
}

// --- One request per entry; persisted data offsets ---

fn span_of(r: &str) -> (u64, u64) {
    let (a, b) = r.strip_prefix("bytes=").unwrap().split_once('-').unwrap();
    (a.parse().unwrap(), b.parse::<u64>().unwrap() + 1)
}

#[tokio::test]
async fn a_first_read_range_of_a_small_entry_is_one_request() {
    let f = fixture().await;
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    f.server.clear_log();
    let id = a.find("LovelyLetter.esp").unwrap();
    assert_eq!(
        a.read_range(id, 0..f.esp.len() as u64).await.unwrap(),
        f.esp
    );
    let gets = file_gets(&f.server);
    assert_eq!(gets.len(), 1, "{gets:?}");
    // It started at the local header.
    let lh = a.index().entries()[id].local_header_offset;
    assert_eq!(span_of(&gets[0]).0, lh);
    // The layout is now known: a later read is the data alone.
    f.server.clear_log();
    assert_eq!(a.read_range(id, 10..20).await.unwrap(), &f.esp[10..20]);
    assert_eq!(file_gets(&f.server).len(), 1);
}

#[tokio::test]
async fn a_frame_mapped_entry_read_from_the_start_is_one_request() {
    let f = fixture().await;
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    f.server.clear_log();
    let id = a.find("Textures/Big.dds").unwrap();
    assert_eq!(
        a.read_range(id, 0..100_000).await.unwrap(),
        &f.big[..100_000]
    );
    let gets = file_gets(&f.server);
    assert_eq!(
        gets.len(),
        1,
        "header and the first frames together: {gets:?}"
    );
    let e = &a.index().entries()[id];
    let (start, end) = span_of(&gets[0]);
    assert_eq!(start, e.local_header_offset);
    let t = e.seek_table.as_ref().unwrap();
    // Two frames' worth (of 64 KiB each), not the whole entry.
    let two: u64 = t.frames()[..2]
        .iter()
        .map(|f| f.compressed_size as u64)
        .sum();
    assert!(end - start < two + 1024, "{gets:?}");
}

#[tokio::test]
async fn a_first_read_in_the_middle_of_an_entry_takes_one_round_trip() {
    let f = fixture().await;
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    f.server.clear_log();
    let id = a.find("Textures/Big.dds").unwrap();
    let r = 200_000..250_000;
    assert_eq!(
        a.read_range(id, r.clone()).await.unwrap(),
        &f.big[r.start as usize..r.end as usize]
    );
    // At most the header and the frames (in parallel), never a header,
    // then the frames, then anything else.
    assert!(
        file_gets(&f.server).len() <= 2,
        "{:?}",
        file_gets(&f.server)
    );
}

#[tokio::test]
async fn persisted_data_offsets_make_reads_skip_the_local_header() {
    let f = fixture().await;
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    let esp = a.find("LovelyLetter.esp").unwrap();
    let big = a.find("Textures/Big.dds").unwrap();
    assert!(a.data_offsets().iter().all(Option::is_none));
    assert_eq!(a.learned(), 0);
    a.read_range(esp, 0..10).await.unwrap();
    a.read_range(big, 0..10).await.unwrap();
    assert_eq!(a.learned(), 2);
    let offsets = a.data_offsets();
    assert!(offsets[esp].is_some() && offsets[big].is_some());
    assert!(offsets[1].is_none(), "a directory was never read");

    f.server.clear_log();
    let b = NexusArchive::from_index_with_offsets(
        f.client.clone(),
        UID,
        a.len(),
        a.index().clone(),
        &offsets,
    )
    .unwrap();
    assert_eq!(b.data_offsets(), offsets);
    assert_eq!(b.learned(), 0, "restored offsets are not news");
    assert_eq!(
        b.read_range(esp, 0..f.esp.len() as u64).await.unwrap(),
        f.esp
    );
    assert_eq!(
        b.read_range(big, 70_000..140_000).await.unwrap(),
        &f.big[70_000..140_000]
    );
    let gets = file_gets(&f.server);
    assert_eq!(gets.len(), 2, "{gets:?}");
    // Exactly the compressed bytes: no local header in either request.
    let e = &b.index().entries()[esp];
    let (s, end) = span_of(&gets[0]);
    assert_eq!((s, end - s), (offsets[esp].unwrap(), e.compressed_size));
    let t = b.index().entries()[big].seek_table.clone().unwrap();
    let want: u64 = t.frames()[1..3]
        .iter()
        .map(|f| f.compressed_size as u64)
        .sum();
    let (s, end) = span_of(&gets[1]);
    assert_eq!(end - s, want);
    assert!(s > offsets[big].unwrap());
}

#[tokio::test]
async fn implausible_persisted_offsets_are_ignored() {
    let f = fixture().await;
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    let esp = a.find("LovelyLetter.esp").unwrap();
    let mut offsets = vec![None; a.index().entries().len()];
    // Before the end of the local header's fixed part, and past the end.
    offsets[esp] = Some(a.index().entries()[esp].local_header_offset + 3);
    offsets[3] = Some(a.len());
    let b = NexusArchive::from_index_with_offsets(
        f.client.clone(),
        UID,
        a.len(),
        a.index().clone(),
        &offsets,
    )
    .unwrap();
    assert!(b.data_offsets().iter().all(Option::is_none));
    assert_eq!(b.read_entry(esp).await.unwrap(), f.esp);
    // A wrong-length offsets list is not an error either: it is ignored.
    let c = NexusArchive::from_index_with_offsets(
        f.client.clone(),
        UID,
        a.len(),
        a.index().clone(),
        &[Some(1)],
    )
    .unwrap();
    assert!(c.data_offsets().iter().all(Option::is_none));
}

/// Incompressible bytes, so compressed offsets track decompressed ones.
fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[tokio::test]
async fn a_local_extra_field_longer_than_the_guess_still_reads_right() {
    let server = start().await;
    let small = data(5_000, 31);
    let big = noise(3 << 20, 32);
    let mut a = RepackFile::new("a.esp", small.clone());
    a.local_extra_pad = 1000;
    let mut b = RepackFile::new("b.dds", big.clone()).frames(256 << 10);
    b.local_extra_pad = 2000;
    server.put_repacked(UID, repacked_zip(&[a, b]));
    let client = Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&server.base)
            .unwrap(),
    );
    let arc = NexusArchive::open(client.clone(), UID).await.unwrap();
    server.clear_log();
    // Header and data in one request, then the few bytes the guess missed.
    assert_eq!(arc.read_range(0, 0..5_000).await.unwrap(), small);
    assert_eq!(file_gets(&server).len(), 2, "{:?}", file_gets(&server));
    // Far into an entry: header and bytes in parallel, then the rest.
    let r = (5 << 19) + 7..(5 << 19) + 100_000;
    assert_eq!(
        arc.read_range(1, r.clone()).await.unwrap(),
        &big[r.start as usize..r.end as usize]
    );
    // Read again from a fresh handle, from the start, to cover the join
    // path of a frame-mapped entry too.
    let fresh = NexusArchive::from_index(client, UID, arc.len(), arc.index().clone()).unwrap();
    assert_eq!(
        fresh.read_range(1, 0..300_000).await.unwrap(),
        &big[..300_000]
    );
    assert_eq!(fresh.data_offsets()[1], arc.data_offsets()[1]);
}

// --- Bulk prefetch: raw spans served from memory ---

fn many_small(n: usize) -> (Vec<RepackFile>, Vec<Vec<u8>>) {
    let bodies: Vec<Vec<u8>> = (0..n)
        .map(|i| data(3_000 + i * 37, 100 + i as u64))
        .collect();
    let files = bodies
        .iter()
        .enumerate()
        .map(|(i, b)| RepackFile::new(&format!("Meshes/m{i:03}.nif"), b.clone()))
        .collect();
    (files, bodies)
}

#[tokio::test]
async fn reads_inside_a_prefetched_span_make_no_requests_of_their_own() {
    let server = start().await;
    let (mut files, bodies) = many_small(30);
    let big = noise(1 << 20, 77);
    files.push(RepackFile::new("Textures/big.dds", big.clone()).frames(128 << 10));
    server.put_repacked(UID, repacked_zip(&files));
    let client = Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&server.base)
            .unwrap(),
    );
    let a = Arc::new(NexusArchive::open(client, UID).await.unwrap());
    // Extents of every small entry and of two ranges of the big one, cover
    // them with one span.
    let mut lo = u64::MAX;
    let mut hi = 0;
    for id in 0..31 {
        let e = &a.index().entries()[id];
        let ranges: Vec<std::ops::Range<u64>> = if id == 30 {
            vec![0..300_000, 600_000..e.uncompressed_size]
        } else {
            std::iter::once(0..e.uncompressed_size).collect()
        };
        for r in ranges {
            let x = a.raw_extent(id, r).unwrap().unwrap();
            lo = lo.min(x.start);
            hi = hi.max(x.end);
        }
    }
    server.clear_log();
    let lease = a.prefetch(lo..hi).unwrap();
    for (id, b) in bodies.iter().enumerate() {
        assert_eq!(&a.read_range(id, 0..b.len() as u64).await.unwrap(), b);
    }
    assert_eq!(a.read_range(30, 0..300_000).await.unwrap(), &big[..300_000]);
    assert_eq!(
        a.read_range(30, 600_000..big.len() as u64).await.unwrap(),
        &big[600_000..]
    );
    let gets = file_gets(&server);
    assert_eq!(gets.len(), 1, "only the span itself: {gets:?}");
    assert_eq!(span_of(&gets[0]), (lo, hi));

    // Released: reads go to the network again.
    drop(lease);
    server.clear_log();
    assert_eq!(&a.read_range(3, 0..10).await.unwrap(), &bodies[3][..10]);
    assert_eq!(file_gets(&server).len(), 1);
}

#[tokio::test]
async fn a_read_across_two_adjacent_spans_is_served_from_both() {
    let server = start().await;
    let big = noise(1 << 20, 78);
    server.put_repacked(
        UID,
        repacked_zip(&[RepackFile::new("big.dds", big.clone()).frames(256 << 10)]),
    );
    let client = Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&server.base)
            .unwrap(),
    );
    let a = Arc::new(NexusArchive::open(client.clone(), UID).await.unwrap());
    let x = a.raw_extent(0, 0..big.len() as u64).unwrap().unwrap();
    let mid = x.start + (x.end - x.start) / 2 + 3;
    server.clear_log();
    let (l1, l2) = (
        a.prefetch(x.start..mid).unwrap(),
        a.prefetch(mid..x.end).unwrap(),
    );
    assert_eq!(a.read_range(0, 0..big.len() as u64).await.unwrap(), big);
    assert_eq!(file_gets(&server).len(), 2, "{:?}", file_gets(&server));
    drop((l1, l2));

    // Overlapping spans: the second supplies only what the first lacks.
    let fresh =
        Arc::new(NexusArchive::from_index(client, UID, a.len(), a.index().clone()).unwrap());
    let l1 = fresh.prefetch(x.start..mid + 5_000).unwrap();
    let l2 = fresh.prefetch(mid - 7_000..x.end).unwrap();
    server.clear_log();
    assert_eq!(fresh.read_range(0, 0..big.len() as u64).await.unwrap(), big);
    assert_eq!(file_gets(&server).len(), 2, "{:?}", file_gets(&server));
    drop((l1, l2));
}

#[tokio::test]
async fn a_failed_prefetch_falls_back_to_ordinary_reads() {
    let f = fixture().await;
    let a = Arc::new(NexusArchive::open(f.client.clone(), UID).await.unwrap());
    let id = a.find("LovelyLetter.esp").unwrap();
    let x = a.raw_extent(id, 0..f.esp.len() as u64).unwrap().unwrap();
    for _ in 0..3 {
        f.server.script(
            &format!("/repacked/{UID}"),
            Canned {
                status: 502,
                headers: vec![],
                body: b"bad gateway".to_vec(),
            },
        );
    }
    let _lease = a.prefetch(x).unwrap();
    assert_eq!(a.read_entry(id).await.unwrap(), f.esp);
    // Past the end of the archive is refused up front.
    assert!(a.prefetch(0..a.len() + 1).is_err());
}

// --- Review fixes ---

#[tokio::test]
async fn a_small_read_of_an_entry_without_a_frame_map_is_not_prefetched_whole() {
    let server = start().await;
    let mut f = RepackFile::new("a.bsa", noise(9 << 20, 81)).frames(1 << 20);
    f.nexus_extra = false;
    server.put_repacked(UID, repacked_zip(&[f]));
    let client = Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&server.base)
            .unwrap(),
    );
    let a = NexusArchive::open(client, UID).await.unwrap();
    // A head read: no bulk extent (it would be the whole entry).
    assert_eq!(a.raw_extent(0, 0..(4 << 20)).unwrap(), None);
    // The whole entry is fine.
    let whole = a.raw_extent(0, 0..(9 << 20)).unwrap().unwrap();
    assert!(whole.end - whole.start > 9 << 20);
}

#[tokio::test]
async fn persisted_offsets_that_cannot_be_right_are_dropped_on_load() {
    let server = start().await;
    let mut stored = RepackFile::new("s.bin", data(500, 3));
    stored.force_stored = true;
    server.put_repacked(
        UID,
        repacked_zip(&[
            RepackFile::new("a.esp", data(5_000, 1)),
            RepackFile::new("b.esp", data(5_000, 2)),
            stored,
        ]),
    );
    let client = Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&server.base)
            .unwrap(),
    );
    let a = NexusArchive::open(client.clone(), UID).await.unwrap();
    for id in 0..3 {
        a.read_entry(id).await.unwrap();
    }
    let good = a.data_offsets();
    let e = a.index().entries();
    let restore = |o: Vec<Option<u64>>| {
        NexusArchive::from_index_with_offsets(client.clone(), UID, a.len(), a.index().clone(), &o)
            .unwrap()
            .data_offsets()
    };
    // Good offsets of compressed entries are kept; stored entries' never
    // (their bytes have no checksum to catch a stale offset).
    assert_eq!(restore(good.clone()), vec![good[0], good[1], None]);
    // Inside the local header's name.
    let early = e[0].local_header_offset + 30 + e[0].name.len() as u64 - 1;
    assert_eq!(restore(vec![Some(early), None, None]), vec![None; 3]);
    // Data that would run into the next entry's local header.
    let late = e[1].local_header_offset - e[0].compressed_size + 1;
    assert_eq!(restore(vec![Some(late), None, None]), vec![None; 3]);
}

#[tokio::test]
async fn reads_say_whether_an_entry_offset_was_restored_unchecked() {
    let f = fixture().await;
    let a = NexusArchive::open(f.client.clone(), UID).await.unwrap();
    let id = a.find("LovelyLetter.esp").unwrap();
    assert!(!a.offset_restored(id));
    a.read_entry(id).await.unwrap();
    assert!(!a.offset_restored(id), "learned from the header");
    let b = NexusArchive::from_index_with_offsets(
        f.client.clone(),
        UID,
        a.len(),
        a.index().clone(),
        &a.data_offsets(),
    )
    .unwrap();
    assert!(b.offset_restored(id));
}
