//! Bulk range bodies from the repacked-file host: exact bytes, read as
//! they arrive, over several clients; a refused link is renewed once; a
//! re-uploaded archive or a short body is an error.
#[path = "../../aether-net/tests/common/mod.rs"]
mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use aether_net::{BulkHttp, BulkHttpConfig, Events, HttpConfig, SourceError};
use aether_nexus::{NexusArchive, NexusClient};
use common::repack::{RepackFile, data, repacked_zip};
use common::{API_KEY, Canned, TestServer, http, start};

const UID: u64 = (1704u64 << 32) + 4242;

async fn fixture() -> (TestServer, Arc<NexusClient>, Vec<u8>) {
    let server = start().await;
    let zip = repacked_zip(&[
        RepackFile::new("a.esp", data(70_000, 1)),
        RepackFile::new("b.dds", data(300_000, 2)).frames(64 << 10),
    ]);
    server.put_repacked(UID, zip.clone());
    let client = Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&server.base)
            .unwrap(),
    );
    (server, client, zip)
}

fn bulk(conns: usize) -> BulkHttp {
    BulkHttp::new(
        &HttpConfig::default(),
        BulkHttpConfig {
            conns,
            ..BulkHttpConfig::default()
        },
    )
    .unwrap()
}

async fn read_all(mut body: aether_net::RangeBody) -> Result<Vec<u8>, SourceError> {
    let mut out = Vec::new();
    while let Some(c) = body.chunk().await? {
        out.extend_from_slice(&c);
    }
    Ok(out)
}

#[tokio::test]
async fn a_range_body_is_exactly_the_bytes_asked_for_over_every_client() {
    let (server, client, zip) = fixture().await;
    let b = bulk(3);
    assert_eq!(b.conns(), 3);
    let len = zip.len() as u64;
    for (i, range) in [
        (0, 0..1000u64),
        (1, 1000..len),
        (2, 17..len - 5),
        (7, 0..len),
    ] {
        let body = client
            .range_body(UID, &b, i, range.clone(), len)
            .await
            .unwrap();
        assert_eq!(body.left(), range.end - range.start);
        let got = read_all(body).await.unwrap();
        assert_eq!(got, &zip[range.start as usize..range.end as usize]);
    }
    // One link for every request; each request one file-host GET.
    assert_eq!(server.requests("/v3/").len(), 1);
    assert_eq!(server.requests("/repacked/").len(), 4);
    // Round-robin.
    let picks: Vec<usize> = (0..6).map(|_| b.next_conn()).collect();
    assert_eq!(picks, [0, 1, 2, 0, 1, 2]);
}

#[tokio::test]
async fn a_refused_link_is_renewed_once_and_the_signature_never_shows() {
    let (server, client, zip) = fixture().await;
    let len = zip.len() as u64;
    let b = bulk(1);
    read_all(client.range_body(UID, &b, 0, 0..10, len).await.unwrap())
        .await
        .unwrap();
    server.state.valid_sig.store(2, Ordering::SeqCst); // the held link now 403s
    let got = read_all(client.range_body(UID, &b, 0, 5..50, len).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got, &zip[5..50]);
    assert_eq!(server.requests("/v3/").len(), 2, "renewed once");
    // Refused for good: the error says so without the signed query.
    server.endless_error(&format!("/repacked/{UID}"), 403);
    let e = client.range_body(UID, &b, 0, 5..50, len).await.unwrap_err();
    assert!(!e.to_string().contains("sig="), "{e}");
}

#[tokio::test]
async fn a_re_uploaded_archive_or_a_bad_range_is_an_error() {
    let (server, client, zip) = fixture().await;
    let len = zip.len() as u64;
    let b = bulk(2);
    // The host now has a longer file: reading at old offsets would be wrong.
    let e = client
        .range_body(UID, &b, 0, 0..10, len - 1)
        .await
        .unwrap_err();
    assert!(matches!(e, SourceError::ArchiveChanged { .. }), "{e}");
    // Out of range: refused before any request.
    let before = server.requests("/repacked/").len();
    assert!(client.range_body(UID, &b, 0, 10..10, len).await.is_err());
    assert!(
        client
            .range_body(UID, &b, 0, 0..len + 1, len)
            .await
            .is_err()
    );
    assert_eq!(server.requests("/repacked/").len(), before);
    // A body that ends early.
    let n = 100usize;
    server.script(
        &format!("/repacked/{UID}"),
        Canned {
            status: 206,
            headers: vec![("content-range", format!("bytes 0-{}/{len}", n - 1))],
            body: zip[..n / 2].to_vec(),
        },
    );
    let r = match client.range_body(UID, &b, 0, 0..n as u64, len).await {
        Ok(body) => read_all(body).await,
        Err(e) => Err(e),
    };
    assert!(r.is_err(), "a short body is never taken for the range");
}

#[tokio::test]
async fn learned_data_offsets_are_kept_only_when_plausible() {
    let (_server, client, zip) = fixture().await;
    let a = NexusArchive::open(client.clone(), UID).await.unwrap();
    let id = a.find("a.esp").unwrap();
    let lh = a.index().entries()[id].local_header_offset;
    let name = a.index().entries()[id].name.len() as u64;
    let before = a.learned();
    a.learn_data_offset(id, lh + 1); // inside the header: refused
    a.learn_data_offset(id, zip.len() as u64); // past the end: refused
    a.learn_data_offset(99, lh); // no such entry
    assert_eq!(a.learned(), before);
    a.learn_data_offset(id, lh + 30 + name + 56);
    assert_eq!(a.learned(), before + 1);
    assert_eq!(a.data_offsets()[id], Some(lh + 30 + name + 56));
}
