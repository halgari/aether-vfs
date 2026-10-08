//! Random-access reads of depot files from the fake CDN.
mod fake_gog;

use std::sync::Arc;

use aether_archive::RangeRead;
use aether_gog::{DepotManifest, GogContent, GogError, Os, ProductId, complete_login};
use aether_net::SourceError;
use fake_gog::{CHUNK, CODE, FakeGog, GAME, big, dlc_esp, http, readme, small_ini, start};

async fn setup(fake: &FakeGog, dir: &std::path::Path) -> (GogContent, Vec<Arc<DepotManifest>>) {
    let cfg = fake.config(dir);
    complete_login(&http(), &cfg, CODE).await.unwrap();
    let content = GogContent::open(http(), cfg).await.unwrap();
    let builds = content.builds(ProductId(GAME), Os::Windows).await.unwrap();
    let details = content.build_details(&builds[0]).await.unwrap();
    let mut manifests = Vec::new();
    for d in &details.depots {
        manifests.push(content.depot(d).await.unwrap());
    }
    (content, manifests)
}

#[tokio::test]
async fn read_spanning_chunks() {
    let fake = start().await;
    let dir = tempfile::tempdir().unwrap();
    let (content, m) = setup(&fake, dir.path()).await;
    let f = content
        .file(ProductId(GAME), &m[0], "data/big.BIN")
        .await
        .unwrap();
    let data = big();
    assert_eq!(f.len(), data.len() as u64);

    fake.clear_log();
    let mut buf = [0u8; 200];
    assert_eq!(f.read_at(4000, &mut buf).await.unwrap(), 200);
    assert_eq!(buf[..], data[4000..4200]);
    assert_eq!(fake.requests("/cdn/").len(), 2, "chunks 0 and 1 only");
    // Cached now: no further requests for them.
    assert_eq!(f.read_at(4090, &mut buf[..10]).await.unwrap(), 10);
    assert_eq!(buf[..10], data[4090..4100]);
    assert_eq!(fake.requests("/cdn/").len(), 2);

    let mut all = vec![0u8; data.len()];
    assert_eq!(f.read_at(0, &mut all).await.unwrap(), data.len());
    assert_eq!(all, data);

    // A single-chunk file, a file in the small-files container, and a file
    // from the DLC's depot (its own product's secure link).
    for (product, manifest, path, want) in [
        (GAME, &m[0], "README.txt", readme()),
        (GAME, &m[0], "Data/Small.ini", small_ini()),
        (fake_gog::DLC, &m[1], "data\\dlc.ESP", dlc_esp()),
    ] {
        let f = content
            .file(ProductId(product), manifest, path)
            .await
            .unwrap();
        let mut got = vec![0u8; want.len()];
        assert_eq!(f.read_at(0, &mut got).await.unwrap(), want.len(), "{path}");
        assert_eq!(got, want, "{path}");
    }
    assert!(
        fake.requests("/cs/products/1207658692/secure_link").len() == 1,
        "{:?}",
        fake.requests("/cs/")
    );

    let e = content
        .file(ProductId(GAME), &m[0], "Data/Missing.esp")
        .await
        .err()
        .unwrap();
    assert!(matches!(e, GogError::NotInDepot(_)), "{e}");
}

#[tokio::test]
async fn read_at_eof_returns_zero() {
    let fake = start().await;
    let dir = tempfile::tempdir().unwrap();
    let (content, m) = setup(&fake, dir.path()).await;
    let f = content
        .file(ProductId(GAME), &m[0], "Data\\Big.bin")
        .await
        .unwrap();
    let len = f.len();
    let mut buf = [0u8; 200];
    assert_eq!(f.read_at(len, &mut buf).await.unwrap(), 0);
    assert_eq!(f.read_at(len + 10, &mut buf).await.unwrap(), 0);
    assert_eq!(f.read_at(u64::MAX, &mut buf).await.unwrap(), 0);
    // A read running past the end is short.
    assert_eq!(f.read_at(len - 5, &mut buf).await.unwrap(), 5);
    assert_eq!(buf[..5], big()[big().len() - 5..]);
    assert_eq!(f.read_at(0, &mut []).await.unwrap(), 0);
}

#[tokio::test]
async fn corrupt_chunk_is_never_served() {
    let fake = start().await;
    let dir = tempfile::tempdir().unwrap();
    let (content, m) = setup(&fake, dir.path()).await;
    let f = content
        .file(ProductId(GAME), &m[0], "Data\\Big.bin")
        .await
        .unwrap();
    let data = big();
    let bad = FakeGog::chunk_id(&data[CHUNK..2 * CHUNK]);
    fake.corrupt(&bad);

    let mut buf = [0u8; 10];
    let e = f.read_at(CHUNK as u64, &mut buf).await.unwrap_err();
    assert!(
        matches!(e, GogError::Source(SourceError::CorruptPart { .. })),
        "{e:?}"
    );
    assert_eq!(fake.chunk_requests(&bad), 3, "retried per the RetryPolicy");
    assert_eq!(buf, [0u8; 10], "nothing written");

    // Other chunks still read.
    assert_eq!(f.read_at(0, &mut buf).await.unwrap(), 10);
    assert_eq!(buf[..], data[..10]);
    // The bad chunk was not cached: once the CDN serves it whole, it reads.
    fake.heal();
    assert_eq!(f.read_at(CHUNK as u64, &mut buf).await.unwrap(), 10);
    assert_eq!(buf[..], data[CHUNK..CHUNK + 10]);
}

#[test]
fn blocking_reader_works_off_the_runtime() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (fake, f) = rt.block_on(async {
        let fake = start().await;
        let (content, m) = setup(&fake, dir.path()).await;
        let f = content
            .file(ProductId(GAME), &m[0], "Data\\Big.bin")
            .await
            .unwrap();
        (fake, f)
    });
    let bf = f.into_blocking(rt.handle().clone());
    assert_eq!(RangeRead::len(&bf), big().len() as u64);
    let t = bf.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 300];
        t.read_at(4000, &mut buf).unwrap();
        assert_eq!(buf[..], big()[4000..4300]);
        // Past the end is UnexpectedEof, as for every RangeRead.
        let e = t
            .read_at(big().len() as u64 - 1, &mut [0u8; 2])
            .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
    })
    .join()
    .unwrap();
    // On a runtime worker: an error, not a hang.
    let r = rt.block_on(async move {
        tokio::spawn(async move { bf.read_at(0, &mut [0u8; 4]) })
            .await
            .unwrap()
    });
    assert!(r.is_err());
    drop(fake);
}

/// Game I/O is many small concurrent reads: those landing in one chunk
/// download it once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reads_of_one_chunk_fetch_it_once() {
    let fake = start().await;
    let dir = tempfile::tempdir().unwrap();
    let (content, m) = setup(&fake, dir.path()).await;
    let f = content
        .file(ProductId(GAME), &m[0], "Data\\Big.bin")
        .await
        .unwrap();
    let data = big();
    fake.delay_cdn(100);
    fake.clear_log();
    let reads = (0..16u64).map(|i| {
        let f = f.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 16];
            let off = CHUNK as u64 + i * 100;
            let n = f.read_at(off, &mut buf).await.unwrap();
            (off, n, buf)
        })
    });
    for r in futures_util::future::join_all(reads).await {
        let (off, n, buf) = r.unwrap();
        assert_eq!(n, 16);
        assert_eq!(buf[..], data[off as usize..off as usize + 16]);
    }
    let id = FakeGog::chunk_id(&data[CHUNK..2 * CHUNK]);
    assert_eq!(fake.chunk_requests(&id), 1, "{:?}", fake.requests("/cdn/"));
}

/// A large read fetches its chunks a few at a time (the `Http`'s
/// `parallel_parts`), not all at once, so it never holds more than that
/// many inflated chunks beyond the cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_read_fetches_a_bounded_number_of_chunks_at_once() {
    let fake = start().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = fake.config(dir.path());
    complete_login(&http(), &cfg, CODE).await.unwrap();
    let content = GogContent::open(fake_gog::http_with_parts(3), cfg)
        .await
        .unwrap();
    let parts: Vec<Vec<u8>> = (0..12u8).map(|i| vec![i; 1000]).collect();
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    let m = fake.manifest(&[("Huge.bin", &refs)]);
    let f = content.file(ProductId(GAME), &m, "huge.bin").await.unwrap();
    fake.delay_cdn(30);
    let mut all = vec![0u8; 12_000];
    assert_eq!(f.read_at(0, &mut all).await.unwrap(), 12_000);
    assert_eq!(all, parts.concat());
    let max = fake.max_cdn_in_flight();
    assert!((2..=3).contains(&max), "{max} chunk requests at once");
}

/// Asserts no CDN path token issued by `fake` appears in `text`.
fn assert_no_token(fake: &FakeGog, text: &str) {
    let tokens = fake.cdn_tokens();
    assert!(!tokens.is_empty());
    for t in tokens {
        assert!(!text.contains(&t), "secure-link token {t} leaked: {text}");
    }
}

/// The secure link carries its token in the URL path, which `redact`
/// (query strings only) does not strip: a failed chunk fetch must not put
/// it in the error, its Debug form, or the retry/failure events.
#[tokio::test]
async fn chunk_errors_never_show_the_secure_link_token() {
    let fake = start().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = fake.config(dir.path());
    complete_login(&http(), &cfg, CODE).await.unwrap();
    let events = aether_net::Events::new(1024);
    let mut rx = events.subscribe();
    let h = aether_net::Http::new(http().config().clone(), events).unwrap();
    let content = GogContent::open(h, cfg).await.unwrap();
    let builds = content.builds(ProductId(GAME), Os::Windows).await.unwrap();
    let details = content.build_details(&builds[0]).await.unwrap();
    let m = content.depot(&details.depots[0]).await.unwrap();
    let f = content
        .file(ProductId(GAME), &m, "Data\\Big.bin")
        .await
        .unwrap();
    let data = big();
    for (i, status) in [(0, 500), (1, 403)] {
        let id = FakeGog::chunk_id(&data[i * CHUNK..(i + 1) * CHUNK]);
        fake.fail_chunk(&id, status);
        let e = f
            .read_at((i * CHUNK) as u64, &mut [0u8; 10])
            .await
            .unwrap_err();
        let shown = format!("{e}");
        assert!(shown.contains(&status.to_string()), "{shown}");
        assert_no_token(&fake, &shown);
        assert_no_token(&fake, &format!("{e:?}"));
        let io: std::io::Error = e.into();
        assert_no_token(&fake, &format!("{io} {io:?}"));
    }
    let mut seen = 0;
    while let Ok(ev) = rx.try_recv() {
        assert_no_token(&fake, &format!("{ev:?}"));
        seen += 1;
    }
    assert!(seen > 0);
}

/// Two downloads refused on the same stale link: the second must not
/// drop the fresh link the first fetched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_refusals_renew_the_secure_link_once() {
    let fake = start().await;
    let dir = tempfile::tempdir().unwrap();
    let (content, m) = setup(&fake, dir.path()).await;
    let f = content
        .file(ProductId(GAME), &m[0], "Data\\Big.bin")
        .await
        .unwrap();
    let data = big();
    let before = fake.secure_link_calls();
    fake.revoke_cdn_tokens();
    // The second chunk's refusal arrives well after the first was renewed.
    fake.delay_chunk(&FakeGog::chunk_id(&data[CHUNK..2 * CHUNK]), 300);
    let reads = (0..2usize).map(|i| {
        let f = f.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; CHUNK];
            f.read_at((i * CHUNK) as u64, &mut buf).await.map(|_| buf)
        })
    });
    for (i, r) in futures_util::future::join_all(reads)
        .await
        .into_iter()
        .enumerate()
    {
        assert_eq!(r.unwrap().unwrap(), data[i * CHUNK..(i + 1) * CHUNK]);
    }
    assert_eq!(fake.secure_link_calls() - before, 1, "one renewal");
}
