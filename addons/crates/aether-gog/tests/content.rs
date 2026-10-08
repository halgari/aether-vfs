//! Builds, build details and depot manifests from the fake content system.
mod fake_gog;

use aether_gog::{GogContent, Os, ProductId, complete_login};
use fake_gog::{BUILD_ID, CODE, DEPOT_A, DEPOT_B, DLC, FakeGog, GAME, big, http, start};

async fn logged_in(fake: &FakeGog, dir: &std::path::Path) -> GogContent {
    let cfg = fake.config(dir);
    complete_login(&http(), &cfg, CODE).await.unwrap();
    GogContent::open(http(), cfg).await.unwrap()
}

#[tokio::test]
async fn builds_and_details_parse() {
    let fake = start().await;
    let dir = tempfile::tempdir().unwrap();
    let content = logged_in(&fake, dir.path()).await;

    let builds = content.builds(ProductId(GAME), Os::Windows).await.unwrap();
    assert_eq!(builds.len(), 1);
    let b = &builds[0];
    assert_eq!(b.build_id.0, BUILD_ID);
    assert_eq!(b.product_id, ProductId(GAME));
    assert_eq!(b.os, Os::Windows);
    assert_eq!(b.version_name, "1.6.1170");
    assert_eq!(b.generation, 2);
    assert_eq!(b.date_published, "2024-01-02T03:04:05+0000");
    assert!(
        fake.requests("/cs/products/1207658691/os/windows/builds")
            .len()
            == 1
    );
    // A product with no builds for this account is an empty list.
    assert!(
        content
            .builds(ProductId(7), Os::Linux)
            .await
            .unwrap()
            .is_empty()
    );

    let details = content.build_details(b).await.unwrap();
    assert_eq!(details.base_product_id, ProductId(GAME));
    assert_eq!(details.install_directory, "Test Game");
    assert_eq!(details.depots.len(), 2);
    let (a, d) = (&details.depots[0], &details.depots[1]);
    assert_eq!(
        (a.product_id, a.manifest.as_str()),
        (ProductId(GAME), DEPOT_A)
    );
    assert_eq!(
        (d.product_id, d.manifest.as_str()),
        (ProductId(DLC), DEPOT_B)
    );
    assert_eq!(d.size, 15);
    assert_eq!(d.languages, ["en-US", "de-DE"]);

    let m = content.depot(a).await.unwrap();
    let paths: Vec<&str> = m.items.iter().map(|i| i.path.as_str()).collect();
    // Directories and links are not files.
    assert_eq!(paths, ["Data\\Big.bin", "Readme.TXT", "Data\\Small.ini"]);
    let big_item = m.find("data/BIG.BIN").unwrap();
    assert_eq!(big_item.size, big().len() as u64);
    assert_eq!(big_item.chunks.len(), 3);
    assert!(big_item.chunks.iter().all(|c| c.size == 4096));
    let small = m.find("Data\\Small.ini").unwrap();
    assert_eq!(small.size, 20);
    assert_eq!(small.flags, ["executable"]);
    assert_eq!(m.small_files_container.as_ref().unwrap().len(), 1);

    let m = content.depot(d).await.unwrap();
    assert_eq!(m.items.len(), 1);
    assert_eq!(m.items[0].path, "Data\\DLC.esp");
}

#[tokio::test]
async fn manifest_cached_on_disk() {
    let fake = start().await;
    let dir = tempfile::tempdir().unwrap();
    let content = logged_in(&fake, dir.path()).await;
    let builds = content.builds(ProductId(GAME), Os::Windows).await.unwrap();
    let details = content.build_details(&builds[0]).await.unwrap();
    let depot = &details.depots[0];

    let meta = format!("/content-system/v2/meta/aa/11/{DEPOT_A}");
    let first = content.depot(depot).await.unwrap();
    assert_eq!(fake.requests(&meta).len(), 1);
    content.depot(depot).await.unwrap();
    assert_eq!(fake.requests(&meta).len(), 1);

    // A new instance (a new process) reads it from disk.
    let again = GogContent::open(http(), fake.config(dir.path()))
        .await
        .unwrap();
    fake.clear_log();
    let second = again.depot(depot).await.unwrap();
    assert!(fake.requests("/").is_empty(), "{:?}", fake.requests("/"));
    assert_eq!(*first, *second);
}
