//! Signed download links kept between runs (the link file): asked for only
//! when a read needs one, then reused until they near their expiry.
#[path = "../../aether-net/tests/common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aether_net::Events;
use aether_nexus::{NexusArchive, NexusClient};
use common::repack::{RepackFile, data, repacked_zip};
use common::{API_KEY, Canned, TestServer, http, start};

const UID: u64 = (1704u64 << 32) + 75329;

fn tmp_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("aether-nexus-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
}

fn link_file(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("index").join("nexus-links.json")
}

/// A client of `s` that keeps its links in `file`, as a new run would
/// build it.
fn client(s: &TestServer, file: &Path) -> Arc<NexusClient> {
    Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&s.base)
            .unwrap()
            .with_link_file(file),
    )
}

fn posts(s: &TestServer) -> usize {
    s.requests("/v3/mod-file-versions/").len()
}

fn posts_for(s: &TestServer, uid: u64) -> usize {
    s.requests(&format!("/v3/mod-file-versions/{uid}/")).len()
}

/// The uids in the link file.
fn saved_uids(file: &Path) -> Vec<u64> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    v["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["uid"].as_u64().unwrap())
        .collect()
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap().as_secs()
}

async fn save(c: &Arc<NexusClient>) {
    let c = c.clone();
    tokio::task::spawn_blocking(move || c.save_links())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_saved_link_is_used_by_the_next_run_without_a_request() {
    let s = start().await;
    s.put_repacked(UID, b"zip bytes".to_vec());
    let dir = tmp_dir();
    let file = link_file(&dir);

    let first = client(&s, &file);
    let url = first.download_url(UID).await.unwrap();
    assert!(!file.exists(), "not one write per link");
    save(&first).await;
    assert_eq!(saved_uids(&file), [UID]);
    assert_eq!(posts(&s), 1);

    let second = client(&s, &file);
    assert!(second.has_url(UID));
    assert!(second.download_url(UID).await.unwrap() == url);
    assert_eq!(posts(&s), 1, "the saved link was used");
    // Nothing changed: nothing is written.
    std::fs::remove_file(&file).unwrap();
    save(&second).await;
    assert!(!file.exists());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn the_link_file_is_private_whatever_was_there() {
    use std::os::unix::fs::PermissionsExt;
    let s = start().await;
    s.put_repacked(UID, b"zip bytes".to_vec());
    let dir = tmp_dir();
    let file = link_file(&dir);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

    let c = client(&s, &file);
    c.download_url(UID).await.unwrap();
    save(&c).await;
    assert_eq!(mode(&file), 0o600);

    // A file someone made readable is replaced by a private one.
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    c.forget_url(UID);
    c.download_url(UID).await.unwrap();
    save(&c).await;
    assert_eq!(mode(&file), 0o600);
    // No temp file is left beside it.
    let names: Vec<_> = std::fs::read_dir(file.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["nexus-links.json"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_within_the_margin_of_expiry_is_neither_saved_nor_loaded() {
    let s = start().await;
    s.put_repacked(UID, b"zip bytes".to_vec());
    let other = UID + 1;
    s.put_repacked(other, b"zip bytes".to_vec());
    let dir = tmp_dir();
    let file = link_file(&dir);

    // Signed with a minute to live: inside the five-minute margin.
    s.state.url_ttl_secs.store(60, Ordering::SeqCst);
    let c = client(&s, &file);
    c.download_url(UID).await.unwrap();
    assert!(!c.has_url(UID));
    save(&c).await;
    assert!(saved_uids(&file).is_empty(), "expired entries are dropped");

    // A file with one link that has four minutes left, one that expired
    // an hour ago and one good for an hour.
    let now = SystemTime::now();
    let entry = |uid: u64, expires: SystemTime| {
        serde_json::json!({
            "uid": uid,
            "url": format!("{}/repacked/{uid}?exp=0&kid=1&sig=1", s.base),
            "expires_at": unix(expires),
        })
    };
    let body = serde_json::json!({ "v": 1, "links": [
        entry(UID, now + Duration::from_secs(240)),
        entry(UID + 2, now - Duration::from_secs(3600)),
        entry(other, now + Duration::from_secs(3600)),
    ]});
    std::fs::write(&file, body.to_string()).unwrap();
    s.clear_log();
    s.state.url_ttl_secs.store(4 * 3600, Ordering::SeqCst);
    let c = client(&s, &file);
    assert!(c.has_url(other));
    assert!(!c.has_url(UID) && !c.has_url(UID + 2));
    c.download_url(other).await.unwrap();
    assert_eq!(posts(&s), 0);
    c.download_url(UID).await.unwrap();
    assert_eq!(posts_for(&s, UID), 1, "too close to expiry to use");
    save(&c).await;
    let mut uids = saved_uids(&file);
    uids.sort_unstable();
    assert_eq!(uids, [UID, other]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_link_file_is_ignored_and_replaced() {
    let s = start().await;
    s.put_repacked(UID, b"zip bytes".to_vec());
    let dir = tmp_dir();
    let file = link_file(&dir);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    for junk in [
        &b"not json at all \xff\xfe"[..],
        br#"{"v":1,"links":[{"uid":"seven"}]}"#,
        // Another format version.
        br#"{"v":99,"links":[]}"#,
    ] {
        std::fs::write(&file, junk).unwrap();
        s.clear_log();
        let c = client(&s, &file);
        assert!(!c.has_url(UID));
        c.download_url(UID).await.unwrap();
        assert_eq!(posts(&s), 1);
        save(&c).await;
        assert_eq!(saved_uids(&file), [UID]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn new_links_are_written_in_batches_while_they_are_added() {
    let s = start().await;
    let uids: Vec<u64> = (0..5).map(|i| UID + i).collect();
    for uid in &uids {
        s.put_repacked(*uid, b"zip bytes".to_vec());
    }
    let dir = tmp_dir();
    let file = link_file(&dir);
    // The default interval (30 s) has not passed: nothing is written.
    let c = client(&s, &file);
    for uid in &uids {
        c.download_url(*uid).await.unwrap();
    }
    assert!(!file.exists());

    // With no interval, a new link is written without being asked for.
    let c = Arc::new(
        NexusClient::new(http(Events::default()), API_KEY)
            .with_base_url(&s.base)
            .unwrap()
            .with_link_file(&file)
            .with_link_save_interval(Duration::ZERO),
    );
    for uid in &uids {
        c.download_url(*uid).await.unwrap();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !file.exists() {
        assert!(std::time::Instant::now() < deadline, "never saved");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Whatever the background save caught, an explicit one has them all.
    save(&c).await;
    let mut saved = saved_uids(&file);
    saved.sort_unstable();
    assert_eq!(saved, uids);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_saved_link_the_file_host_rejects_is_signed_again() {
    let s = start().await;
    let esp = data(4000, 1);
    s.put_repacked(UID, repacked_zip(&[RepackFile::new("a.esp", esp.clone())]));
    let dir = tmp_dir();
    let file = link_file(&dir);
    let first = client(&s, &file);
    first.download_url(UID).await.unwrap();
    save(&first).await;

    // The file host stops honouring the saved link before it expires.
    s.state.valid_sig.store(2, Ordering::SeqCst);
    s.clear_log();
    let second = client(&s, &file);
    assert!(second.has_url(UID), "the saved link looks valid");
    let a = NexusArchive::open(second.clone(), UID).await.unwrap();
    assert_eq!(a.read_entry(a.find("a.esp").unwrap()).await.unwrap(), esp);
    assert_eq!(posts(&s), 1, "one new link after the 403");

    // The new link replaces the rejected one in the file.
    save(&second).await;
    s.clear_log();
    let third = client(&s, &file);
    let a = NexusArchive::open(third, UID).await.unwrap();
    assert_eq!(a.read_entry(a.find("a.esp").unwrap()).await.unwrap(), esp);
    assert_eq!(posts(&s), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_link_shows_in_debug_output() {
    let s = start().await;
    s.put_repacked(UID, b"zip bytes".to_vec());
    let dir = tmp_dir();
    let file = link_file(&dir);
    let c = client(&s, &file);
    c.download_url(UID).await.unwrap();
    save(&c).await;
    let c = client(&s, &file);
    let text = format!("{c:?}");
    assert!(!text.contains("sig=") && !text.contains("repacked"));
    assert!(!text.contains(API_KEY));
}

fn link_path(uid: u64) -> String {
    format!("/v3/mod-file-versions/{uid}/download-repacked")
}

/// A link response as the API sends it, with these headers.
fn link_with(s: &TestServer, uid: u64, headers: &[(&'static str, &str)]) -> Canned {
    let expires = SystemTime::now() + Duration::from_secs(4 * 3600);
    let body = serde_json::json!({
        "download_url": format!("{}/repacked/{uid}?exp=0&kid=1&sig=1", s.base),
        "expires_at": humantime::format_rfc3339_seconds(expires).to_string(),
    });
    Canned {
        status: 200,
        headers: headers.iter().map(|(k, v)| (*k, v.to_string())).collect(),
        body: body.to_string().into_bytes(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_client_remembers_the_allowance_the_api_last_reported() {
    let s = start().await;
    let uids: Vec<u64> = (0..6).map(|i| UID + i).collect();
    for uid in &uids[..5] {
        s.put_repacked(*uid, b"zip bytes".to_vec());
    }
    let dir = tmp_dir();
    let c = client(&s, &link_file(&dir));
    assert_eq!(c.quota(), None, "nothing was asked yet");

    // An API that reports nothing: links are signed all the same.
    c.download_url(uids[0]).await.unwrap();
    assert_eq!(c.quota(), None);

    s.set_quota((2000, 1500), (20_000, 12_000));
    c.download_url(uids[1]).await.unwrap();
    let q = c.quota().unwrap();
    let left = |q: &aether_nexus::Quota| {
        let (h, d) = (q.hourly.unwrap(), q.daily.unwrap());
        assert_eq!((h.limit, d.limit), (2000, 20_000));
        (h.remaining, d.remaining)
    };
    assert_eq!(left(&q), (1499, 11_999));
    assert_eq!(q.spare(), 499);

    // A link from the cache is no request: the reading stands.
    c.download_url(uids[1]).await.unwrap();
    assert_eq!(left(&c.quota().unwrap()), (1499, 11_999));
    // Any call counts, and so does a refused one (a file Nexus lacks).
    c.validate().await.unwrap();
    assert_eq!(left(&c.quota().unwrap()), (1498, 11_998));
    assert!(c.download_url(uids[5]).await.is_err());
    assert_eq!(left(&c.quota().unwrap()), (1497, 11_997));

    // Headers that say nothing usable leave the last reading in place,
    // and the link that came with them is used like any other.
    s.no_quota();
    s.script(
        &link_path(uids[2]),
        link_with(
            &s,
            uids[2],
            &[
                ("x-rl-hourly-limit", "lots"),
                ("x-rl-hourly-remaining", "-3"),
                ("x-rl-daily-remaining", ""),
            ],
        ),
    );
    c.download_url(uids[2]).await.unwrap();
    assert!(c.has_url(uids[2]));
    assert_eq!(left(&c.quota().unwrap()), (1497, 11_997));
    // So does a response with no such header.
    c.download_url(uids[4]).await.unwrap();
    assert_eq!(left(&c.quota().unwrap()), (1497, 11_997));

    // A reading of one bucket replaces that bucket alone.
    s.script(
        &link_path(uids[3]),
        link_with(
            &s,
            uids[3],
            &[
                ("x-rl-hourly-limit", "2000"),
                ("x-rl-hourly-remaining", "7"),
            ],
        ),
    );
    c.download_url(uids[3]).await.unwrap();
    let q = c.quota().unwrap();
    assert_eq!(left(&q), (7, 11_997));
    assert_eq!(q.spare(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_gets_its_link_whatever_is_left_of_the_allowance() {
    let s = start().await;
    let esp = data(4000, 1);
    s.put_repacked(UID, repacked_zip(&[RepackFile::new("a.esp", esp.clone())]));
    let dir = tmp_dir();
    let c = client(&s, &link_file(&dir));
    // Almost nothing left, far below half.
    s.set_quota((2000, 3), (20_000, 5));
    c.validate().await.unwrap();
    assert_eq!(c.quota().unwrap().spare(), 0);
    let a = NexusArchive::open(c.clone(), UID).await.unwrap();
    assert_eq!(a.read_entry(a.find("a.esp").unwrap()).await.unwrap(), esp);
    assert_eq!(posts(&s), 1);
    assert_eq!(s.quota_left(), (1, 3));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_refusal_of_the_old_link_does_not_drop_the_new_one() {
    let s = start().await;
    s.put_repacked(UID, b"zip bytes".to_vec());
    let dir = tmp_dir();
    let c = client(&s, &link_file(&dir));
    let old = c.download_url(UID).await.unwrap();
    // The host revokes it; the first reader to be refused signs again.
    s.state.valid_sig.store(2, Ordering::SeqCst);
    c.forget_refused(UID, &old);
    assert!(!c.has_url(UID));
    let new = c.download_url(UID).await.unwrap();
    assert!(new != old);
    // A second reader's refusal, of the old link, arrives after that.
    c.forget_refused(UID, &old);
    assert!(c.has_url(UID), "the fresh link stays");
    assert!(c.download_url(UID).await.unwrap() == new);
    assert_eq!(posts(&s), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn readers_refused_together_sign_one_new_link() {
    let s = start().await;
    let esp = data(4000, 1);
    s.put_repacked(UID, repacked_zip(&[RepackFile::new("a.esp", esp.clone())]));
    let dir = tmp_dir();
    let c = client(&s, &link_file(&dir));
    let a = Arc::new(NexusArchive::open(c.clone(), UID).await.unwrap());
    let id = a.find("a.esp").unwrap();
    s.state.valid_sig.store(2, Ordering::SeqCst);
    s.clear_log();
    let reads: Vec<_> = (0..16)
        .map(|_| {
            let a = a.clone();
            tokio::spawn(async move { a.read_entry(id).await.unwrap() })
        })
        .collect();
    for r in reads {
        assert_eq!(r.await.unwrap(), esp);
    }
    assert_eq!(posts(&s), 1, "one new link for all sixteen");
}

/// A dead link the host refuses with another status than 403 must not
/// outlive a restart: it is in the link file.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_link_refused_with_another_status_than_403_is_signed_again() {
    for status in [401u16, 410] {
        let s = start().await;
        let esp = data(4000, 1);
        s.put_repacked(UID, repacked_zip(&[RepackFile::new("a.esp", esp.clone())]));
        let dir = tmp_dir();
        let file = link_file(&dir);
        let first = client(&s, &file);
        let a = NexusArchive::open(first.clone(), UID).await.unwrap();
        save(&first).await;
        drop(a);

        let second = client(&s, &file);
        let a = NexusArchive::open(second.clone(), UID).await.unwrap();
        s.clear_log();
        s.script(
            &format!("/repacked/{UID}"),
            Canned {
                status,
                headers: vec![],
                body: b"gone".to_vec(),
            },
        );
        assert_eq!(a.read_entry(a.find("a.esp").unwrap()).await.unwrap(), esp);
        assert_eq!(posts(&s), 1, "HTTP {status}: a new link, then the read");
    }
}

/// The host being busy, or a range it cannot serve, says nothing about
/// the link: it is kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_transient_failure_does_not_forget_the_link() {
    let s = start().await;
    let esp = data(4000, 1);
    s.put_repacked(UID, repacked_zip(&[RepackFile::new("a.esp", esp.clone())]));
    let dir = tmp_dir();
    let c = client(&s, &link_file(&dir));
    let a = NexusArchive::open(c.clone(), UID).await.unwrap();
    s.clear_log();
    let script = |status: u16| {
        s.script(
            &format!("/repacked/{UID}"),
            Canned {
                status,
                headers: vec![("retry-after", "0".into())],
                body: b"later".to_vec(),
            },
        )
    };
    let id = a.find("a.esp").unwrap();
    // Retried, and served.
    for status in [503u16, 429] {
        script(status);
        assert_eq!(a.read_entry(id).await.unwrap(), esp);
        assert!(c.has_url(UID), "HTTP {status}");
    }
    // Not retried: the read fails, the link stays.
    for status in [408u16, 416] {
        script(status);
        assert!(a.read_entry(id).await.is_err());
        assert!(c.has_url(UID), "HTTP {status}");
    }
    assert_eq!(posts(&s), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn temp_files_a_killed_save_left_behind_are_removed_at_load() {
    let s = start().await;
    s.put_repacked(UID, b"zip bytes".to_vec());
    let dir = tmp_dir();
    let file = link_file(&dir);
    let index = file.parent().unwrap();
    std::fs::create_dir_all(index).unwrap();
    let stale = index.join("nexus-links.json.4242.7.partial");
    std::fs::write(&stale, br#"{"v":1,"links":[]}"#).unwrap();
    // Not ours: left alone.
    let other = index.join("nexus-links.json.bak");
    let partial_of_other = index.join("other.json.1.2.partial");
    std::fs::write(&other, b"x").unwrap();
    std::fs::write(&partial_of_other, b"x").unwrap();
    let c = client(&s, &file);
    assert!(!stale.exists());
    assert!(other.exists() && partial_of_other.exists());
    c.download_url(UID).await.unwrap();
    save(&c).await;
    assert_eq!(saved_uids(&file), [UID]);
}
