//! Live tests against real GOG. Ignored by default; they need a saved login
//! (`GOG_CREDENTIALS`: a file written by `complete_login`) and an owned
//! product (`GOG_PRODUCT`, default 1207658691). Run with
//! `GOG_CREDENTIALS=… cargo test -p aether-gog --test live -- --ignored --nocapture`.
use std::path::PathBuf;

use aether_gog::{GogConfig, GogContent, GogCredentials, Os, ProductId};
use aether_net::{Events, Http, HttpConfig};

fn product() -> ProductId {
    ProductId(
        std::env::var("GOG_PRODUCT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1207658691),
    )
}

/// The config and saved login, or `None` (skip) without `GOG_CREDENTIALS`.
fn setup(cache: &std::path::Path) -> Option<(GogConfig, GogCredentials)> {
    let Some(path) = std::env::var_os("GOG_CREDENTIALS").map(PathBuf::from) else {
        eprintln!("skipping: GOG_CREDENTIALS is not set");
        return None;
    };
    let creds = GogCredentials::load(&path)
        .unwrap()
        .expect("GOG_CREDENTIALS names no login file");
    Some((GogConfig::new(cache, path), creds))
}

fn http() -> Http {
    Http::new(HttpConfig::default(), Events::default()).unwrap()
}

#[tokio::test]
#[ignore = "network: needs a saved GOG login"]
async fn refreshes_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let Some((cfg, mut creds)) = setup(dir.path()) else {
        return;
    };
    let old = creds.refresh_token().to_string();
    creds.expires_at = 0; // force a refresh before the first request
    let content = GogContent::with_credentials(http(), cfg.clone(), creds);
    content.builds(product(), Os::Windows).await.unwrap();
    let saved = GogCredentials::load(&cfg.credentials).unwrap().unwrap();
    assert!(saved.expires_at > 0);
    eprintln!(
        "refreshed; refresh token {}",
        if saved.refresh_token() == old {
            "unchanged"
        } else {
            "rotated"
        }
    );
}

#[tokio::test]
#[ignore = "network: needs a saved GOG login owning GOG_PRODUCT"]
async fn lists_builds() {
    let dir = tempfile::tempdir().unwrap();
    let Some((cfg, creds)) = setup(dir.path()) else {
        return;
    };
    let content = GogContent::with_credentials(http(), cfg, creds);
    let builds = content.builds(product(), Os::Windows).await.unwrap();
    assert!(!builds.is_empty());
    for b in builds.iter().take(5) {
        eprintln!(
            "{} {} {} gen {}",
            b.build_id, b.version_name, b.date_published, b.generation
        );
    }
}

#[tokio::test]
#[ignore = "network: needs a saved GOG login owning GOG_PRODUCT"]
async fn reads_the_first_64k_of_a_depot_file() {
    let dir = tempfile::tempdir().unwrap();
    let Some((cfg, creds)) = setup(dir.path()) else {
        return;
    };
    let content = GogContent::with_credentials(http(), cfg, creds);
    let builds = content.builds(product(), Os::Windows).await.unwrap();
    let details = content.build_details(&builds[0]).await.unwrap();
    let depot = &details.depots[0];
    let manifest = content.depot(depot).await.unwrap();
    let item = manifest
        .items
        .iter()
        .find(|i| i.size > 0)
        .expect("a non-empty file");
    eprintln!("{} ({} bytes)", item.path, item.size);
    let f = content
        .file(depot.product_id, &manifest, &item.path)
        .await
        .unwrap();
    let mut buf = vec![0u8; 64 << 10];
    let n = f.read_at(0, &mut buf).await.unwrap();
    assert_eq!(n as u64, item.size.min(64 << 10));
    // A whole single-chunk file can be checked against the manifest's MD5.
    if let Some(md5) = item.md5
        && n as u64 == item.size
    {
        use md5::Digest;
        let got: [u8; 16] = md5::Md5::digest(&buf[..n]).into();
        assert_eq!(got, md5);
    }
}
