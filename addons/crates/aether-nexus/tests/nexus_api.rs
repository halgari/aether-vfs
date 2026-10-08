#[path = "../../aether-net/tests/common/mod.rs"]
mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use aether_net::{Events, SourceError, SourceEvent};
use aether_nexus::{NexusClient, SKYRIM_SE_GAME_ID, uid};
use common::{API_KEY, Canned, TestServer, http, start};

const UID: u64 = (1704u64 << 32) + 75329;

struct Fixture {
    server: TestServer,
    client: Arc<NexusClient>,
    events: Events,
}

async fn fixture() -> Fixture {
    let server = start().await;
    // The API only hands out links for files the file host has.
    server.put_repacked(UID, b"zip bytes".to_vec());
    let events = Events::new(4096);
    let client = Arc::new(
        NexusClient::new(http(events.clone()), API_KEY)
            .with_base_url(&server.base)
            .unwrap(),
    );
    Fixture {
        server,
        client,
        events,
    }
}

fn posts(s: &TestServer) -> usize {
    s.requests("/v3/mod-file-versions/").len()
}

#[tokio::test]
async fn download_urls_are_cached_until_close_to_expiry() {
    let f = fixture().await;
    let a = f.client.download_url(UID).await.unwrap();
    let b = f.client.download_url(UID).await.unwrap();
    assert_eq!(a, b);
    assert_eq!(posts(&f.server), 1);
    assert!(a.as_str().contains(&format!("/repacked/{UID}?")));

    f.client.forget_url(UID);
    f.server.state.url_ttl_secs.store(60, Ordering::SeqCst); // inside the 5-minute margin
    f.client.download_url(UID).await.unwrap();
    f.client.download_url(UID).await.unwrap();
    assert_eq!(posts(&f.server), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_download_url_calls_make_one_request() {
    let f = fixture().await;
    let client = f.client.clone();
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let client = client.clone();
            tokio::spawn(async move { client.download_url(UID).await.unwrap() })
        })
        .collect();
    let mut urls = Vec::new();
    for t in tasks {
        urls.push(t.await.unwrap());
    }
    assert!(urls.iter().all(|u| *u == urls[0]));
    assert_eq!(posts(&f.server), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_calls_near_expiry_make_one_renewal() {
    let f = fixture().await;
    // Cache a URL that is already inside the five-minute renewal margin.
    f.server.state.url_ttl_secs.store(60, Ordering::SeqCst);
    f.client.download_url(UID).await.unwrap();
    assert_eq!(posts(&f.server), 1);
    // Later responses are long-lived again, so whichever caller renews it
    // gives every other waiter a fresh-enough cached URL.
    f.server
        .state
        .url_ttl_secs
        .store(4 * 3600, Ordering::SeqCst);

    let client = f.client.clone();
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let client = client.clone();
            tokio::spawn(async move { client.download_url(UID).await.unwrap() })
        })
        .collect();
    let mut urls = Vec::new();
    for t in tasks {
        urls.push(t.await.unwrap());
    }
    assert!(urls.iter().all(|u| *u == urls[0]));
    assert_eq!(posts(&f.server), 2, "one renewal, not eight");
}

#[tokio::test]
async fn api_errors_are_actionable() {
    let f = fixture().await;
    let bad = NexusClient::new(http(Events::default()), "wrong")
        .with_base_url(&f.server.base)
        .unwrap();
    let e = bad.download_url(UID).await.unwrap_err();
    assert!(matches!(e, SourceError::NexusUnauthorized));
    assert!(e.to_string().contains("Log in to Nexus Mods again"));

    let path = format!("/v3/mod-file-versions/{UID}/download-repacked");
    f.server.script(
        &path,
        Canned {
            status: 403,
            headers: vec![("content-type", "application/problem+json".into())],
            body: br#"{"status":403,"detail":"Premium membership required"}"#.to_vec(),
        },
    );
    let e = f.client.download_url(UID).await.unwrap_err();
    assert!(e.to_string().contains("Premium"), "{e}");

    let e = f
        .client
        .download_url(uid(SKYRIM_SE_GAME_ID, 1))
        .await
        .unwrap_err();
    assert!(matches!(e, SourceError::NotFound { .. }), "{e}");
}

#[tokio::test]
async fn rate_limits_are_retried_then_reported() {
    let f = fixture().await;
    let mut rx = f.events.subscribe();
    let path = format!("/v3/mod-file-versions/{UID}/download-repacked");
    let limited = || Canned {
        status: 429,
        headers: vec![("retry-after", "0".into())],
        body: Vec::new(),
    };
    f.server.script(&path, limited());
    f.client.download_url(UID).await.unwrap();
    let retries = std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|e| matches!(e, SourceEvent::Retry { .. }))
        .count();
    assert_eq!(retries, 1);

    f.client.forget_url(UID);
    for _ in 0..3 {
        f.server.script(&path, limited());
    }
    let e = f.client.download_url(UID).await.unwrap_err();
    assert!(matches!(e, SourceError::RateLimited { .. }), "{e}");
}

#[tokio::test]
async fn file_record_gives_the_uid() {
    let f = fixture().await;
    let r = f
        .client
        .file_record("skyrimspecialedition", 75329)
        .await
        .unwrap();
    assert_eq!(r.uid, UID);
    assert_eq!(r.uid, uid(SKYRIM_SE_GAME_ID, 75329));
}

#[tokio::test]
async fn an_endless_api_error_body_is_not_read_to_the_end() {
    let f = fixture().await;
    f.server.endless_error(
        &format!("/v3/mod-file-versions/{UID}/download-repacked"),
        400,
    );
    let e = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        f.client.download_url(UID),
    )
    .await
    .expect("error body must not be read to the end")
    .unwrap_err();
    assert!(matches!(&e, SourceError::Status { status: 400, .. }), "{e}");
}

#[tokio::test]
async fn the_api_key_is_never_sent_across_a_redirect() {
    let f = fixture().await;
    f.server.redirect(
        &format!("/v3/mod-file-versions/{UID}/download-repacked"),
        &f.server.url("/elsewhere"),
    );
    let e = f.client.download_url(UID).await.unwrap_err();
    assert!(matches!(&e, SourceError::Status { status: 302, .. }), "{e}");
    assert!(f.server.requests("/elsewhere").is_empty());
}
