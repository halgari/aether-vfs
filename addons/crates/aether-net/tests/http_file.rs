mod common;

use aether_archive::Xxh64;
use aether_net::{Events, HttpFile, MemorySink, SourceError, SourceEvent};
use common::repack::data;
use common::{Canned, http, start};

#[tokio::test]
async fn ranged_servers_are_downloaded_in_parallel_chunks() {
    let server = start().await;
    let bytes = data((1 << 20) + 17, 11);
    server.put("/f/a.7z", bytes.clone());
    let f = HttpFile::new(http(Events::default()), &server.url("/f/a.7z")).unwrap();
    let sink = MemorySink::new();
    let got = f
        .download(sink.clone(), Some(Xxh64::of(&bytes)), None)
        .await
        .unwrap();
    assert_eq!((got.len, got.hash), (bytes.len() as u64, Xxh64::of(&bytes)));
    assert_eq!(sink.contents(), bytes);
    let reqs = server.requests("/f/a.7z");
    // One probe, then 17 chunks of 64 KiB.
    assert_eq!(reqs.len(), 1 + 17);
    assert!(reqs.iter().all(|r| r.range.is_some()));
}

#[tokio::test]
async fn servers_without_range_get_one_stream() {
    let server = start().await;
    let bytes = data(300_000, 12);
    server.put("/f/b.zip", bytes.clone());
    server.no_range("/f/b.zip");
    let f = HttpFile::new(http(Events::default()), &server.url("/f/b.zip")).unwrap();
    let sink = MemorySink::new();
    f.download(sink.clone(), Some(Xxh64::of(&bytes)), None)
        .await
        .unwrap();
    assert_eq!(sink.contents(), bytes);
    assert_eq!(server.requests("/f/b.zip").len(), 2);
}

#[tokio::test]
async fn redirects_are_followed() {
    let server = start().await;
    let bytes = data(200_000, 13);
    server.put("/assets/c.7z", bytes.clone());
    server.redirect("/releases/c.7z", &server.url("/assets/c.7z"));
    let f = HttpFile::new(http(Events::default()), &server.url("/releases/c.7z")).unwrap();
    let sink = MemorySink::new();
    f.download(sink.clone(), None, None).await.unwrap();
    assert_eq!(sink.contents(), bytes);
}

#[tokio::test]
async fn transient_errors_are_retried() {
    let server = start().await;
    let bytes = data(100_000, 14);
    server.put("/f/d.7z", bytes.clone());
    server.script(
        "/f/d.7z",
        Canned {
            status: 503,
            headers: vec![],
            body: vec![],
        },
    );
    let f = HttpFile::new(http(Events::default()), &server.url("/f/d.7z")).unwrap();
    let sink = MemorySink::new();
    f.download(sink.clone(), None, None).await.unwrap();
    assert_eq!(sink.contents(), bytes);
}

#[tokio::test]
async fn hash_mismatch_and_missing_files_fail() {
    let server = start().await;
    server.put("/f/e.bin", data(1000, 15));
    let f = HttpFile::new(http(Events::default()), &server.url("/f/e.bin")).unwrap();
    let e = f
        .download(MemorySink::new(), Some(Xxh64(42)), None)
        .await
        .unwrap_err();
    assert!(matches!(e, SourceError::HashMismatch { .. }), "{e}");
    let g = HttpFile::new(http(Events::default()), &server.url("/f/none")).unwrap();
    let e = g.download(MemorySink::new(), None, None).await.unwrap_err();
    assert!(matches!(e, SourceError::NotFound { .. }), "{e}");
}

#[tokio::test]
async fn empty_files_download() {
    let server = start().await;
    server.put("/f/empty", Vec::new());
    let f = HttpFile::new(http(Events::default()), &server.url("/f/empty")).unwrap();
    let got = f
        .download(MemorySink::new(), Some(Xxh64::of(b"")), None)
        .await
        .unwrap();
    assert_eq!(got.len, 0);
}

#[tokio::test]
async fn an_endless_error_body_is_not_read_to_the_end() {
    let server = start().await;
    server.endless_error("/f/huge-error", 400);
    let f = HttpFile::new(http(Events::default()), &server.url("/f/huge-error")).unwrap();
    let e = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        f.download(MemorySink::new(), None, None),
    )
    .await
    .expect("error body must not be read to the end")
    .unwrap_err();
    assert!(
        matches!(&e, SourceError::Status { status: 400, body, .. } if body.len() <= 300),
        "{e}"
    );
}

#[tokio::test]
async fn expected_length_is_checked_up_front_and_reported_as_the_total() {
    let server = start().await;
    let bytes = data(200_000, 16);
    server.put("/f/g.7z", bytes.clone());
    server.put("/f/h.7z", bytes.clone());
    server.no_range("/f/h.7z");
    let len = bytes.len() as u64;

    let events = Events::new(4096);
    let mut rx = events.subscribe();
    let f = HttpFile::new(http(events), &server.url("/f/g.7z")).unwrap();
    f.download(MemorySink::new(), None, Some(len))
        .await
        .unwrap();
    let evs: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        evs.iter()
            .any(|e| matches!(e, SourceEvent::Started { total: Some(t), .. } if *t == len)),
        "{evs:?}"
    );

    // Ranged: refused after the size probe, before any chunk.
    server.clear_log();
    let sink = MemorySink::new();
    let e = f
        .download(sink.clone(), None, Some(len + 1))
        .await
        .unwrap_err();
    assert!(matches!(e, SourceError::Protocol { .. }), "{e}");
    assert_eq!(server.requests("/f/g.7z").len(), 1);
    assert!(sink.contents().is_empty());

    // Unranged: refused on the declared Content-Length.
    let f = HttpFile::new(http(Events::default()), &server.url("/f/h.7z")).unwrap();
    let sink = MemorySink::new();
    let e = f
        .download(sink.clone(), None, Some(len - 1))
        .await
        .unwrap_err();
    assert!(matches!(e, SourceError::Protocol { .. }), "{e}");
    assert!(sink.contents().is_empty());
    f.download(MemorySink::new(), None, Some(len))
        .await
        .unwrap();
}
