mod common;

use aether_net::{Events, SourceError};
use common::{Canned, http, start};

#[tokio::test]
async fn get_bytes_returns_the_body_and_retries_transient_errors() {
    let server = start().await;
    server.put("/repos.json", br#"{"a":"b"}"#.to_vec());
    server.script(
        "/repos.json",
        Canned {
            status: 503,
            headers: vec![],
            body: vec![],
        },
    );
    let got = http(Events::default())
        .get_bytes(&server.url("/repos.json"), 1024)
        .await
        .unwrap();
    assert_eq!(got, br#"{"a":"b"}"#);
    assert_eq!(server.requests("/repos.json").len(), 2);
}

#[tokio::test]
async fn get_bytes_follows_redirects() {
    let server = start().await;
    server.put("/real.json", b"[]".to_vec());
    server.redirect("/moved.json", &server.url("/real.json"));
    let got = http(Events::default())
        .get_bytes(&server.url("/moved.json"), 1024)
        .await
        .unwrap();
    assert_eq!(got, b"[]");
}

#[tokio::test]
async fn get_bytes_refuses_oversized_bodies_and_missing_files() {
    let server = start().await;
    server.put("/big.json", vec![b' '; 2000]);
    let h = http(Events::default());
    let e = h
        .get_bytes(&server.url("/big.json"), 1000)
        .await
        .unwrap_err();
    assert!(matches!(e, SourceError::Protocol { .. }), "{e}");
    let e = h
        .get_bytes(&server.url("/gone.json?token=SECRET"), 1000)
        .await
        .unwrap_err();
    assert!(matches!(e, SourceError::NotFound { .. }), "{e}");
    assert!(!e.to_string().contains("SECRET"), "{e}");
    let e = h.get_bytes("ht!tp://x/y?sig=SECRET", 10).await.unwrap_err();
    assert!(!e.to_string().contains("SECRET"), "{e}");
}
