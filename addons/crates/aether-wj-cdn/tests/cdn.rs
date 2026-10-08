#[path = "../../aether-net/tests/common/mod.rs"]
mod common;

use std::io::Write;

use aether_archive::Xxh64;
use aether_net::{Events, MemorySink, SourceError, SourceEvent};
use aether_wj_cdn::CdnFile;
use common::repack::data;
use common::{Canned, TestServer, http, start};

const FILE: &str = "/The%20List%20-%20Output.7z_d412aa87";
const PART: usize = 64 << 10;

/// Serve `bytes` as a CDN file split into `PART`-byte parts.
fn put_cdn(server: &TestServer, bytes: &[u8]) {
    let parts: Vec<_> = bytes
        .chunks(PART)
        .enumerate()
        .map(|(i, c)| {
            server.put(&format!("{FILE}/parts/{i}"), c.to_vec());
            serde_json::json!({
                "Size": c.len(), "Offset": i * PART, "Hash": Xxh64::of(c).to_base64(), "Index": i,
            })
        })
        .collect();
    let def = serde_json::json!({
        "Author": "test", "OriginalFileName": "The List - Output.7z", "Size": bytes.len(),
        "Hash": Xxh64::of(bytes).to_base64(), "Parts": parts,
        "ServerAssignedUniqueId": "d412aa87", "MungedName": "The List - Output.7z_d412aa87",
    });
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(def.to_string().as_bytes()).unwrap();
    server.put(&format!("{FILE}/definition.json.gz"), gz.finish().unwrap());
}

#[tokio::test]
async fn downloads_and_verifies_all_parts() {
    let server = start().await;
    let bytes = data(5 * PART + 123, 7);
    put_cdn(&server, &bytes);
    let events = Events::new(4096);
    let mut rx = events.subscribe();
    let cdn = CdnFile::new(http(events), &server.url(FILE)).unwrap();
    let sink = MemorySink::new();
    let def = cdn
        .download(sink.clone(), Some(Xxh64::of(&bytes)))
        .await
        .unwrap();
    assert_eq!(def.parts.len(), 6);
    assert_eq!(sink.contents(), bytes);
    assert_eq!(server.requests(&format!("{FILE}/parts/")).len(), 6);
    let evs: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    let progress: u64 = evs
        .iter()
        .filter_map(|e| match e {
            SourceEvent::Progress { bytes, .. } => Some(*bytes),
            _ => None,
        })
        .sum();
    assert!(progress >= bytes.len() as u64);
    assert!(evs.iter().any(
        |e| matches!(e, SourceEvent::Started { total: Some(t), .. } if *t == bytes.len() as u64)
    ));
    assert_eq!(
        evs.iter()
            .filter(|e| matches!(e, SourceEvent::Finished { .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn a_corrupt_part_is_fetched_again() {
    let server = start().await;
    let bytes = data(3 * PART, 8);
    put_cdn(&server, &bytes);
    server.script(
        &format!("{FILE}/parts/1"),
        Canned {
            status: 200,
            headers: vec![],
            body: vec![0u8; PART],
        },
    );
    let cdn = CdnFile::new(http(Events::default()), &server.url(FILE)).unwrap();
    let sink = MemorySink::new();
    cdn.download(sink.clone(), None).await.unwrap();
    assert_eq!(sink.contents(), bytes);
    assert_eq!(server.requests(&format!("{FILE}/parts/1")).len(), 2);
}

#[tokio::test]
async fn whole_file_hash_must_match_the_modlist() {
    let server = start().await;
    let bytes = data(PART + 1, 9);
    put_cdn(&server, &bytes);
    let cdn = CdnFile::new(http(Events::default()), &server.url(FILE)).unwrap();
    let e = cdn
        .download(MemorySink::new(), Some(Xxh64(1)))
        .await
        .unwrap_err();
    assert!(matches!(e, SourceError::HashMismatch { .. }), "{e}");
}

#[tokio::test]
async fn a_missing_file_is_not_found() {
    let server = start().await;
    let cdn = CdnFile::new(http(Events::default()), &server.url("/nope")).unwrap();
    assert!(matches!(
        cdn.definition().await,
        Err(SourceError::NotFound { .. })
    ));
}

#[tokio::test]
async fn a_definition_with_an_oversized_part_is_refused_before_any_part_is_fetched() {
    let server = start().await;
    let size = (64u64 << 20) + 1;
    let def = serde_json::json!({
        "OriginalFileName": "x.7z", "Size": size, "Hash": Xxh64(1).to_base64(),
        "Parts": [{"Size": size, "Offset": 0, "Hash": Xxh64(1).to_base64(), "Index": 0}],
    });
    server.put(
        &format!("{FILE}/definition.json.gz"),
        def.to_string().into_bytes(),
    );
    let cdn = CdnFile::new(http(Events::default()), &server.url(FILE)).unwrap();
    let e = cdn.download(MemorySink::new(), None).await.unwrap_err();
    assert!(matches!(e, SourceError::Protocol { .. }), "{e}");
    assert!(server.requests(&format!("{FILE}/parts/")).is_empty());
}

#[tokio::test]
async fn a_query_is_sent_but_never_shown() {
    let server = start().await;
    let bytes = data(2 * PART + 5, 10);
    put_cdn(&server, &bytes);
    let events = Events::new(4096);
    let mut rx = events.subscribe();
    let url = format!("{}?token=SECRET#frag", server.url(FILE));
    let cdn = CdnFile::new(http(events.clone()), &url).unwrap();
    assert!(!cdn.url().contains("SECRET"), "{}", cdn.url());
    assert!(!format!("{cdn:?}").contains("SECRET"), "{cdn:?}");
    let sink = MemorySink::new();
    cdn.download(sink.clone(), Some(Xxh64::of(&bytes)))
        .await
        .unwrap();
    assert_eq!(
        sink.contents(),
        bytes,
        "the parts were found under the path"
    );
    let evs: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(evs.iter().any(|e| matches!(e, SourceEvent::Started { .. })));
    for e in &evs {
        assert!(!format!("{e:?}").contains("SECRET"), "{e:?}");
    }

    // A failure's error and events never show it either.
    let mut rx = events.subscribe();
    let missing = CdnFile::new(
        http(events),
        &format!("{}?token=SECRET", server.url("/nope")),
    )
    .unwrap();
    let e = missing.download(MemorySink::new(), None).await.unwrap_err();
    assert!(matches!(e, SourceError::NotFound { .. }), "{e}");
    assert!(!e.to_string().contains("SECRET"), "{e}");
    let evs: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(!evs.is_empty());
    for e in &evs {
        assert!(!format!("{e:?}").contains("SECRET"), "{e:?}");
    }
}
