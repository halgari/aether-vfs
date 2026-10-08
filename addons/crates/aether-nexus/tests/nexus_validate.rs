#[path = "../../aether-net/tests/common/mod.rs"]
mod common;

use aether_net::{Events, SourceError};
use aether_nexus::{NexusClient, NexusUser};
use common::{API_KEY, Canned, TestServer, http, start};

const PATH: &str = "/v1/users/validate.json";

fn client(server: &TestServer, key: &str) -> NexusClient {
    NexusClient::new(http(Events::default()), key)
        .with_base_url(&server.base)
        .unwrap()
}

#[tokio::test]
async fn a_valid_key_gives_the_user() {
    let server = start().await;
    let user = client(&server, API_KEY).validate().await.unwrap();
    assert_eq!(
        user,
        NexusUser {
            name: "Tester".into(),
            is_premium: true
        }
    );
    let seen = server.requests(PATH);
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "GET");
    assert!(!format!("{user:?}").contains(API_KEY));
}

#[tokio::test]
async fn a_free_account_is_reported_not_rejected() {
    let server = start().await;
    server.script(
        PATH,
        Canned {
            status: 200,
            headers: vec![],
            body: br#"{"key":"test-key","name":"Freeloader","is_premium":false}"#.to_vec(),
        },
    );
    let user = client(&server, API_KEY).validate().await.unwrap();
    assert_eq!(user.name, "Freeloader");
    assert!(!user.is_premium);
}

#[tokio::test]
async fn a_wrong_key_is_unauthorized_and_never_shown() {
    let server = start().await;
    let secret = "WRONG-SECRET-KEY-42";
    let c = client(&server, secret);
    let e = c.validate().await.unwrap_err();
    assert!(matches!(e, SourceError::NexusUnauthorized), "{e}");
    for s in [e.to_string(), format!("{e:?}"), format!("{c:?}")] {
        assert!(!s.contains(secret), "{s}");
    }
}

#[tokio::test]
async fn a_malformed_reply_never_echoes_the_key() {
    let server = start().await;
    // A wrongly typed field makes serde quote the value it saw; here that
    // value is the key.
    server.script(
        PATH,
        Canned {
            status: 200,
            headers: vec![],
            body: br#"{"name":"Tester","is_premium":"test-key"}"#.to_vec(),
        },
    );
    let e = client(&server, API_KEY).validate().await.unwrap_err();
    assert!(matches!(e, SourceError::Protocol { .. }), "{e}");
    for s in [e.to_string(), format!("{e:?}")] {
        assert!(!s.contains(API_KEY), "{s}");
    }
}
