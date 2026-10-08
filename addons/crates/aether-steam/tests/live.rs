//! Live tests against real Steam. Ignored by default; run with
//! `cargo test -p aether-steam --test live -- --ignored --nocapture`.
use aether_steam::{
    AppId, CdnConfig, CredentialFile, DepotId, SessionConfig, SteamCache, SteamContent, SteamGame,
    SteamSession,
};

/// Anonymous: Spacewar (app 480, depot 481) is the one depot an anonymous
/// session may decrypt. Covers CM connect, PICS, depot key, request code,
/// CDN list, manifest download/parse/name decryption, range reads and cache.
#[tokio::test]
#[ignore = "network: anonymous Steam"]
async fn anonymous_spacewar_end_to_end() {
    let session = SteamSession::anonymous(SessionConfig::default())
        .await
        .unwrap();
    let manifests = session
        .branch_manifests(AppId(480), "public")
        .await
        .unwrap();
    let (_, id) = *manifests
        .iter()
        .find(|(d, _)| *d == DepotId(481))
        .expect("depot 481");
    let dir = tempfile::tempdir().unwrap();
    let cache = SteamCache::new(dir.path());
    let content = SteamContent::new(
        cache.clone(),
        None,
        SessionConfig::default(),
        CdnConfig::default(),
    )
    .with_session(session);
    let game = SteamGame::from_depots(&content, AppId(480), &[(DepotId(481), id)])
        .await
        .unwrap();
    let (_, entry) = game
        .locate("DejaVuSans.ttf")
        .expect("Spacewar ships DejaVuSans.ttf");
    let sha = entry.sha1.expect("manifest has a file SHA-1");
    let f = game.open_file("dejavusans.TTF").unwrap();
    let all = f.read_range(0, f.len()).await.unwrap();
    assert_eq!(sha1_smol::Sha1::from(&all).digest().bytes(), sha);
    let mid = f.len() / 3;
    assert_eq!(
        f.read_range(mid, 4096).await.unwrap(),
        all[mid as usize..mid as usize + 4096]
    );
    // Everything is cached now: a fresh instance with no session reads too.
    let offline = SteamContent::new(cache, None, SessionConfig::default(), CdnConfig::default());
    let game = SteamGame::from_depots(&offline, AppId(480), &[(DepotId(481), id)])
        .await
        .unwrap();
    let f = game.open_file("DejaVuSans.ttf").unwrap();
    assert_eq!(f.read_range(0, 64).await.unwrap(), all[..64]);
}

/// The ticket Bethesda.net's `external-login` takes, minted by Haskill's
/// own session. Prints only its length.
#[tokio::test]
#[ignore = "network: needs a saved Haskill Steam login that owns Skyrim SE"]
async fn encrypted_app_ticket_for_skyrim_se() {
    let file = match std::env::var_os("HASKILL_STEAM_LOGIN") {
        Some(p) => CredentialFile::new(p),
        None => CredentialFile::new(CredentialFile::default_path().unwrap()),
    };
    let Some(creds) = file.load().unwrap() else {
        eprintln!("skipping: no Haskill Steam login");
        return;
    };
    let s = SteamSession::login(&creds, SessionConfig::default())
        .await
        .unwrap();
    let t = s.encrypted_app_ticket(AppId(489830)).await.unwrap();
    let n = t.bytes().len();
    eprintln!("ticket: {n} bytes");
    assert!((100..400).contains(&n), "{n}");
}

/// The licence check Creations need: this account owns Skyrim SE and the
/// Anniversary Upgrade.
#[tokio::test]
#[ignore = "network: needs a saved Haskill Steam login that owns the Anniversary Upgrade"]
async fn owns_the_anniversary_upgrade() {
    let file = match std::env::var_os("HASKILL_STEAM_LOGIN") {
        Some(p) => CredentialFile::new(p),
        None => CredentialFile::new(CredentialFile::default_path().unwrap()),
    };
    let Some(creds) = file.load().unwrap() else {
        eprintln!("skipping: no Haskill Steam login");
        return;
    };
    let s = SteamSession::login(&creds, SessionConfig::default())
        .await
        .unwrap();
    assert!(s.owns_app(AppId(489830)).await.unwrap());
    assert!(s.owns_app(AppId(1746860)).await.unwrap());
    assert!(!s.owns_app(AppId(1691340)).await.unwrap(), "Steel Hunters");
}
