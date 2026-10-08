//! Login, token refresh and its failure, against the fake auth server.
mod fake_gog;

use aether_gog::{GogContent, GogCredentials, GogError, Os, ProductId, complete_login, login_url};
use fake_gog::{CLIENT_ID, CODE, GAME, REDIRECT, USER_ID, http, start};

#[tokio::test]
async fn login_accepts_redirect_url_or_bare_code() {
    let fake = start().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = fake.config(dir.path());

    let url = login_url(&cfg);
    assert_eq!(url.path(), "/auth/auth");
    let q: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    for (k, v) in [
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT),
        ("response_type", "code"),
        ("layout", "client2"),
    ] {
        assert!(q.contains(&(k.into(), v.into())), "{k} missing from {url}");
    }
    assert!(!url.as_str().contains("secret"), "{url}");

    let h = http();
    let pasted = format!("https://embed.gog.com/on_login_success?origin=client&code={CODE}");
    let a = complete_login(&h, &cfg, &pasted).await.unwrap();
    let from_url = fake.last_token_query();
    let b = complete_login(&h, &cfg, &format!("  {CODE}\n"))
        .await
        .unwrap();
    let from_code = fake.last_token_query();
    assert_eq!(from_url, from_code);
    for (k, v) in [
        ("grant_type", "authorization_code"),
        ("code", CODE),
        ("redirect_uri", REDIRECT),
    ] {
        assert!(from_url.contains(&(k.into(), v.into())), "{from_url:?}");
    }
    assert_eq!(fake.code_calls(), 2);
    assert_eq!((a.user_id.as_str(), b.user_id.as_str()), (USER_ID, USER_ID));
    assert!(!format!("{a:?}").contains("access-"), "{a:?}");

    // The second login is the one on disk, owner-only.
    let saved = GogCredentials::load(&cfg.credentials).unwrap().unwrap();
    assert_eq!(saved, b);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&cfg.credentials)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // A redirect URL without a code, or nothing at all, is refused before
    // any request; a code GOG rejects is a login error.
    for bad in [
        "",
        "   ",
        "https://embed.gog.com/on_login_success?origin=client",
    ] {
        let e = complete_login(&h, &cfg, bad).await.unwrap_err();
        assert!(matches!(e, GogError::Login(_)), "{bad:?}: {e}");
    }
    assert_eq!(fake.code_calls(), 2);
    let e = complete_login(&h, &cfg, "wrong-code").await.unwrap_err();
    assert!(matches!(e, GogError::Login(_)), "{e}");
}

#[tokio::test]
async fn expired_token_refreshes_once() {
    let fake = start().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = fake.config(dir.path());
    let h = http();
    complete_login(&h, &cfg, CODE).await.unwrap();
    let content = GogContent::open(h, cfg.clone()).await.unwrap();

    fake.expire_access_tokens();
    let builds = content.builds(ProductId(GAME), Os::Windows).await.unwrap();
    assert_eq!(builds.len(), 1);
    assert_eq!(fake.refresh_calls(), 1);
    // Two tries of the builds request: the rejected one and the retry.
    assert_eq!(fake.requests("/cs/products/").len(), 2);

    // The new tokens are saved and used from now on.
    let saved = GogCredentials::load(&cfg.credentials).unwrap().unwrap();
    assert_eq!(saved.refresh_token(), "refresh-2");
    content.builds(ProductId(GAME), Os::Windows).await.unwrap();
    assert_eq!(fake.refresh_calls(), 1);
}

#[tokio::test]
async fn refresh_failure_is_login_expired() {
    let fake = start().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = fake.config(dir.path());
    let h = http();
    complete_login(&h, &cfg, CODE).await.unwrap();
    let content = GogContent::open(h, cfg).await.unwrap();

    fake.expire_access_tokens();
    fake.fail_refresh();
    let e = content
        .builds(ProductId(GAME), Os::Windows)
        .await
        .unwrap_err();
    let GogError::LoginExpired(msg) = &e else {
        panic!("expected LoginExpired, got {e:?}");
    };
    assert!(msg.contains("log in to GOG again"), "{msg}");
    assert!(e.to_string().contains("log in to GOG again"), "{e}");
    assert_eq!(fake.refresh_calls(), 1, "one refresh, no loop");
}

#[tokio::test]
async fn open_without_a_login_is_not_logged_in() {
    let fake = start().await;
    let dir = tempfile::tempdir().unwrap();
    let e = GogContent::open(http(), fake.config(dir.path()))
        .await
        .err()
        .unwrap();
    assert!(matches!(e, GogError::NotLoggedIn), "{e}");
}
