//! Download a Steam game's files to a directory, as a client would install
//! them, with the login the `steam_login` example saved.
//!
//! ```text
//! cargo run -p aether-steam --example steam_download -- APP DEST [options]
//! cargo run -p aether-steam --example steam_download -- APP --list
//!
//!   --list            show the branch's depots and which would be installed
//!   --os OS           the client OS whose depots to install (default windows)
//!   --lang LANGUAGE   the client language (default english)
//!   --depots A,B,…    exactly these depots, in this order (earlier wins)
//!   --branch NAME     the branch (default public)
//!   --jobs N          files downloaded at once (default 8)
//! ```
//!
//! Reads the login from `$AETHER_STEAM_LOGIN`. Manifests and depot keys are
//! cached in `$AETHER_STEAM_CACHE` (default: `cache/` beside the login file).
//!
//! Resumable and checked: a file already in `DEST` with the manifest's size
//! and SHA-1 is kept; anything else is fetched to `<file>.part`, its SHA-1
//! checked, and renamed into place. A depot this account may not read (a DLC
//! it does not own) is reported and skipped.
use aether_steam::{
    AppId, BranchDepot, CdnConfig, CredentialFile, DepotId, SessionConfig, SteamCache,
    SteamContent, SteamError, SteamGame, SteamSession,
};
use futures_util::stream::{self, StreamExt};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Bytes asked of a file at once.
const PIECE: u64 = 32 << 20;

struct Opts {
    app: AppId,
    dest: Option<PathBuf>,
    list: bool,
    os: String,
    lang: String,
    depots: Option<Vec<u32>>,
    branch: String,
    jobs: usize,
}

fn usage() -> ! {
    eprintln!(
        "usage: steam_download APP DEST [--os OS] [--lang LANG] [--depots A,B] \
         [--branch NAME] [--jobs N]\n       steam_download APP --list"
    );
    std::process::exit(2)
}

fn parse() -> Opts {
    let mut args = std::env::args().skip(1);
    let app = args
        .next()
        .and_then(|a| a.parse().ok())
        .map(AppId)
        .unwrap_or_else(|| usage());
    let mut o = Opts {
        app,
        dest: None,
        list: false,
        os: "windows".into(),
        lang: "english".into(),
        depots: None,
        branch: "public".into(),
        jobs: 8,
    };
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--list" => o.list = true,
            "--os" => o.os = val(),
            "--lang" => o.lang = val(),
            "--branch" => o.branch = val(),
            "--jobs" => o.jobs = val().parse().unwrap_or_else(|_| usage()),
            "--depots" => {
                o.depots = Some(
                    val()
                        .split(',')
                        .map(|d| d.trim().parse().unwrap_or_else(|_| usage()))
                        .collect(),
                )
            }
            d if !d.starts_with('-') && o.dest.is_none() => o.dest = Some(d.into()),
            _ => usage(),
        }
    }
    if !o.list && o.dest.is_none() {
        usage()
    }
    o
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    if let Err(e) = run(parse()).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(o: Opts) -> Result<(), SteamError> {
    let Some(login) = std::env::var_os("AETHER_STEAM_LOGIN").map(PathBuf::from) else {
        eprintln!("set AETHER_STEAM_LOGIN to the login file steam_login wrote");
        std::process::exit(2);
    };
    let creds = CredentialFile::new(&login)
        .load()?
        .ok_or(SteamError::NotLoggedIn)?;
    let cache_dir = std::env::var_os("AETHER_STEAM_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| login.with_file_name("cache"));
    let session = SteamSession::login(&creds, SessionConfig::default()).await?;
    eprintln!("Logged in as {}.", session.account().unwrap_or("?"));
    if !session.owns_app(o.app).await? {
        eprintln!(
            "warning: this account does not appear to own app {}",
            o.app.0
        );
    }

    let all = session.branch_depots(o.app, &o.branch).await?;
    let chosen: Vec<BranchDepot> = match &o.depots {
        Some(ids) => ids
            .iter()
            .map(|id| {
                all.iter()
                    .find(|d| d.depot.0 == *id)
                    .cloned()
                    .ok_or_else(|| SteamError::Protocol(format!("no depot {id} on {}", o.branch)))
            })
            .collect::<Result<_, _>>()?,
        None => all
            .iter()
            .filter(|d| d.installed_for(&o.os, &o.lang))
            .cloned()
            .collect(),
    };
    eprintln!("Depots of app {} on {}:", o.app.0, o.branch);
    for d in &all {
        let mark = if chosen.iter().any(|c| c.depot == d.depot) {
            "*"
        } else {
            " "
        };
        eprintln!(
            " {mark} {:>8}  os={:<16} lang={:<10} {}",
            d.depot.0,
            if d.os_list.is_empty() {
                "any".to_string()
            } else {
                d.os_list.join(",")
            },
            d.language
                .as_deref()
                .filter(|l| !l.is_empty())
                .unwrap_or("any"),
            d.name.as_deref().unwrap_or("")
        );
    }
    if o.list {
        return Ok(());
    }
    let dest = o.dest.clone().expect("checked in parse");

    let content = SteamContent::new(
        SteamCache::new(cache_dir),
        Some(creds),
        SessionConfig::default(),
        CdnConfig::default(),
    )
    .with_session(session);
    // One depot at a time, so one this account cannot read is skipped
    // rather than failing the game.
    let mut readable = Vec::new();
    for d in &chosen {
        match SteamGame::from_depots(&content, o.app, &[(d.depot, d.manifest)]).await {
            Ok(_) => readable.push((d.depot, d.manifest)),
            Err(e) => eprintln!("skipping depot {}: {e}", d.depot.0),
        }
    }
    let game = SteamGame::from_depots(&content, o.app, &readable).await?;
    let files: Vec<(DepotId, aether_steam::FileEntry)> = game
        .files()
        .into_iter()
        .map(|(d, f)| (d, f.clone()))
        .collect();
    let total: u64 = files.iter().map(|(_, f)| f.size).sum();
    eprintln!(
        "{} files, {:.2} GiB, into {}",
        files.len(),
        total as f64 / (1u64 << 30) as f64,
        dest.display()
    );

    let done = Arc::new(AtomicU64::new(0));
    let fetched = Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    let progress = {
        let (done, fetched) = (done.clone(), fetched.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let d = done.load(Ordering::Relaxed);
                let f = fetched.load(Ordering::Relaxed);
                let secs = start.elapsed().as_secs_f64().max(1.0);
                eprintln!(
                    "progress: {:.1}% ({:.2} of {:.2} GiB), {:.1} MiB/s downloaded",
                    100.0 * d as f64 / total.max(1) as f64,
                    d as f64 / (1u64 << 30) as f64,
                    total as f64 / (1u64 << 30) as f64,
                    f as f64 / (1u64 << 20) as f64 / secs
                );
            }
        })
    };

    let game = &game;
    let results: Vec<Result<(), String>> = stream::iter(files.iter())
        .map(|(_, f)| {
            let (dest, done, fetched) = (&dest, done.clone(), fetched.clone());
            async move {
                fetch(game, f, dest, &done, &fetched)
                    .await
                    .map_err(|e| format!("{}: {e}", f.path))
            }
        })
        .buffer_unordered(o.jobs.max(1))
        .collect()
        .await;
    progress.abort();
    let failed: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    for e in &failed {
        eprintln!("failed: {e}");
    }
    eprintln!(
        "{} of {} files in place in {:.0}s",
        files.len() - failed.len(),
        files.len(),
        start.elapsed().as_secs_f64()
    );
    if failed.is_empty() {
        Ok(())
    } else {
        Err(SteamError::Protocol(format!(
            "{} files failed",
            failed.len()
        )))
    }
}

/// `f` into `dest`, unless it is already there with the right size and
/// SHA-1.
async fn fetch(
    game: &SteamGame,
    f: &aether_steam::FileEntry,
    dest: &Path,
    done: &AtomicU64,
    fetched: &AtomicU64,
) -> Result<(), SteamError> {
    let rel: PathBuf = f.path.split('\\').collect();
    let path = dest.join(rel);
    if already_there(&path, f)? {
        done.fetch_add(f.size, Ordering::Relaxed);
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let part = path.with_file_name(format!(
        "{}.part",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let mut out = std::fs::File::create(&part)?;
    let mut sha = sha1_smol::Sha1::new();
    let file = game.open_file(&f.path)?;
    let mut off = 0;
    while off < f.size {
        let len = PIECE.min(f.size - off);
        let bytes = file.read_range(off, len).await?;
        sha.update(&bytes);
        out.write_all(&bytes)?;
        off += len;
        done.fetch_add(len, Ordering::Relaxed);
        fetched.fetch_add(len, Ordering::Relaxed);
    }
    out.sync_all()?;
    drop(out);
    if let Some(want) = f.sha1
        && sha.digest().bytes() != want
    {
        let _ = std::fs::remove_file(&part);
        return Err(SteamError::Integrity(format!("{}: SHA-1 mismatch", f.path)));
    }
    std::fs::rename(&part, &path)?;
    Ok(())
}

/// Whether `path` already holds `f`: the same size, and the same SHA-1 when
/// the manifest has one.
fn already_there(path: &Path, f: &aether_steam::FileEntry) -> Result<bool, SteamError> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() && m.len() == f.size => {}
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    let Some(want) = f.sha1 else {
        return Ok(true);
    };
    let mut sha = sha1_smol::Sha1::new();
    let mut r = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sha.update(&buf[..n]);
    }
    Ok(sha.digest().bytes() == want)
}
