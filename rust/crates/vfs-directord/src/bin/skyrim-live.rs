//! Live launch: Skyrim SE from a Stored zip (no extract) with remapped
//! saves/profiles and a write overlay.
//!
//! ```text
//! cargo run -p vfs-directord --bin skyrim-live --release
//! ```
//!
//! Defaults (override with env):
//! - `VFS_SKYRIM_ZIP`     = `C:\tmp\skyrimse.zip`
//! - `VFS_SKYRIM_DATA`    = `C:\tmp\skyrim-data`  (saves/, profiles/, overrides/)
//! - `VFS_SKYRIM_ROOT`    = `C:\tmp\skyrim-runtime` (empty managed virtual root)
//! - `VFS_SKYRIM_LAUNCH`  = `SkyrimSE.exe`, or `skse64_loader.exe` to go via SKSE

use std::path::{Path, PathBuf};
use std::sync::Arc;

use vfs_compose::StripPrefixBackend;
use vfs_director::{Backend, DiskBackend, LaunchOpts, Session};
use vfs_protocol::KIND_DIR;
use vfs_zip::ZipBackend;

fn env_path(key: &str, default: &str) -> PathBuf {
    std::env::var_os(key)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Reads whole files out of the composed VFS for [`vfs_director::stage`].
struct KernelSource<'a>(&'a Session);

impl vfs_director::stage::ImageSource for KernelSource<'_> {
    fn read(&self, vpath: &str) -> Option<Vec<u8>> {
        let k = self.0.kernel();
        let (fh, size, _) = k.open(vpath, vfs_protocol::OPEN_READ).ok()?;
        let mut buf = vec![0u8; size as usize];
        let mut off = 0usize;
        while off < buf.len() {
            match k.read(fh, off as u64, &mut buf[off..]) {
                Ok(0) => break,
                Ok(n) => off += n,
                Err(_) => {
                    let _ = k.close(fh);
                    return None;
                }
            }
        }
        let _ = k.close(fh);
        buf.truncate(off);
        Some(buf)
    }
}

fn run() -> Result<(), String> {
    // Load benchmark: record phase timings and, when VFS_BENCH=1, stop at the
    // first rendered frame instead of running the game indefinitely.
    let mut timeline = vfs_director::bench::Timeline::new();
    let bench = std::env::var("VFS_BENCH").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false);

    let zip_path = env_path("VFS_SKYRIM_ZIP", r"C:\tmp\skyrimse.zip");
    let data = env_path("VFS_SKYRIM_DATA", r"C:\tmp\skyrim-data");
    let state = data.join("vfs-state");
    let saves = data.join("saves");
    let profiles = data.join("profiles");
    let overrides = data.join("overrides");

    if !zip_path.is_file() {
        return Err(format!("zip not found: {}", zip_path.display()));
    }

    // The image is staged from the zip just before launch (see `stage_launch`
    // below), so no Steam install is required or consulted. Measured 2026-08-12:
    // with the Steam library deleted entirely, the game runs and DRM passes from
    // an image under C:\tmp — Steam associates the process via steam_appid.txt
    // and the running client, not the image path. `VFS_LAUNCH_IMAGE` overrides,
    // for bisecting against a real install.
    let preset_host = std::env::var("VFS_LAUNCH_IMAGE").ok().filter(|h| Path::new(h).is_file());

    // Managed root: never the Steam tree. Content is served here from the zip.
    let root = if let Some(r) = std::env::var_os("VFS_SKYRIM_ROOT") {
        PathBuf::from(r)
    } else {
        PathBuf::from(r"C:\tmp\skyrim-runtime")
    };
    let stage_root = data.join("stage");

    // Optional mod overlay (e.g. an unpacked SKSE) composed over the zip, and
    // the executable to launch — `skse64_loader.exe` to go through SKSE.
    let mods_dir = std::env::var_os("VFS_SKYRIM_MODS").map(PathBuf::from);
    let launch_exe =
        std::env::var("VFS_SKYRIM_LAUNCH").unwrap_or_else(|_| "SkyrimSE.exe".to_string());

    eprintln!("skyrim-live");
    eprintln!("  zip:       {}", zip_path.display());
    if let Some(m) = &mods_dir {
        eprintln!("  mods:      {}  (overlay above zip)", m.display());
    }
    eprintln!("  launch:    {launch_exe}");
    match &preset_host {
        Some(h) => eprintln!("  image:     {h}  (VFS_LAUNCH_IMAGE override)"),
        None => eprintln!("  image:     staged from zip into {}", stage_root.display()),
    }
    eprintln!("  root:      {}  (managed VFS root)", root.display());
    eprintln!("  overrides: {}", overrides.display());
    eprintln!("  saves:     {}", saves.display());
    eprintln!("  profiles:  {}", profiles.display());

    // ── host dirs (never wipe the Steam library) ───────────────────────────
    for d in [&state, &saves, &profiles, &overrides] {
        std::fs::create_dir_all(d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
    }
    if is_safe_to_wipe(&root) {
        wipe_files(&root)?;
    } else {
        eprintln!("  skip wipe of Steam/library root {}", root.display());
    }
    // DX redist next to the host exe (LoadLibrary search / SetDllDirectory).
    stage_dx_redist(&root, &data.join("dx-redist"))?;

    // ── remap My Games saves + profile/ini area ────────────────────────────
    setup_my_games_junctions(&profiles, &saves)?;
    setup_localappdata_junction(&profiles)?;

    // DRM path: talk to the *already-running* Steam client only.
    // - Never set SteamAppId/SteamGameId env — that makes RestartAppIfNecessary
    //   hand off to steam://run (reopen Steam UI / Remote Play).
    // - Do write steam_appid.txt next to the exe (and in the VFS overlay). That
    //   is Valve's documented dev override so RestartAppIfNecessary returns
    //   false and SteamAPI_Init still verifies ownership via the running client.
    std::env::remove_var("SteamAppId");
    std::env::remove_var("SteamGameId");
    std::env::remove_var("SteamOverlayGameId");
    std::env::remove_var("SteamClientLaunch");
    std::env::remove_var("SteamEnv");
    std::env::remove_var("SteamTenfoot");
    std::env::remove_var("SteamAppUser");
    write_steam_appid(&root, &overrides)?;
    // Overlay inject has been observed to trip Steam CM asserts (`Expected
    // connection state 0/1 but got 2`) and clean Shutdown.
    // Best-effort: per-app EnableGameOverlay=0 in localconfig + registry.
    disable_skyrim_game_overlay()?;
    ensure_steam_running()?;
    ensure_steam_logged_on()?;
    eprintln!("  steam: AppId env cleared; steam_appid.txt=489830; client must stay running");
    // Keep host steam_api64 for DRM IPC only. All other game content must come
    // from the zip via the director — never open the Steam library tree.
    //
    // Default on, but honour an explicit setting. `VFS_KEEP_HOST_STEAM_API=0`
    // serves steam_api* from the zip instead.
    if std::env::var_os("VFS_KEEP_HOST_STEAM_API").is_none() {
        std::env::set_var("VFS_KEEP_HOST_STEAM_API", "1");
    }
    std::env::remove_var("VFS_ALLOW_DISK_FALLTHROUGH");
    std::env::remove_var("VFS_DISK_ONLY_ROOT");
    if let Some(p) = std::env::var_os("VFS_DIRECTOR_OPEN_LOG") {
        eprintln!("  director-open log: {}", p.to_string_lossy());
    }

    // ── base content: a directory tree, or the zip ──────────────────────────
    // `VFS_SKYRIM_DISK` swaps the archive for an already-extracted install. The
    // point is differential diagnosis: it keeps the shim, the staging, the
    // sealed root and the launch path identical while changing only where bytes
    // come from, so a behaviour that survives the swap is not the archive's.
    let disk_src = std::env::var_os("VFS_SKYRIM_DISK").map(PathBuf::from);
    let backend: Arc<dyn Backend> = if let Some(d) = &disk_src {
        if !d.is_dir() {
            return Err(format!("VFS_SKYRIM_DISK not a directory: {}", d.display()));
        }
        eprintln!("  base:      {}  (disk tree, zip bypassed)", d.display());
        timeline.mark("zip index");
        Arc::new(DiskBackend::new(d))
    } else {
        // Open the zip ONCE. (A second open re-scans the 16GB CD and looks frozen.)
        eprintln!("  opening zip index (one-time CD parse; may take ~30–90s on a 16GB archive)…");
        let t0 = std::time::Instant::now();
        let zip = ZipBackend::open(&zip_path).map_err(|e| format!("ZipBackend: {e:?}"))?;
        eprintln!("  zip index ready in {:.1}s", t0.elapsed().as_secs_f32());
        timeline.mark("zip index");

        // Detect single top-level folder from the already-open backend (no re-open).
        let prefix = detect_zip_root_prefix(&zip)?;
        eprintln!("  zip root prefix: {prefix:?}");
        if let Some(pfx) = prefix {
            Arc::new(StripPrefixBackend::new(Arc::new(zip), pfx))
        } else {
            Arc::new(zip)
        }
    };

    let mut session = Session::new();
    session.set_root(&root);
    session.set_overlay(&overrides);
    session.set_state_dir(&state);
    // Composition: zip (game content) under overrides (steam_appid, writes).
    // Do **not** mount the Steam library DiskBackend — that let the game load
    // masters/BSAs/DLLs from the host install and violated the VFS contract.
    // steam_appid.txt is written into `overrides` before launch.
    session
        .mount("", backend)
        .map_err(|st| format!("mount zip status {st}"))?;
    // Mods sit above the base game and below the write overlay, so an unpacked
    // SKSE contributes skse64_loader.exe / skse64_*.dll at the root and merges
    // its Data/Scripts into the game's Data.
    if let Some(m) = &mods_dir {
        if !m.is_dir() {
            return Err(format!("VFS_SKYRIM_MODS not a directory: {}", m.display()));
        }
        session
            .mount("", Arc::new(DiskBackend::new(m)))
            .map_err(|st| format!("mount mods status {st}"))?;
    }
    session
        .mount("", Arc::new(DiskBackend::new(&overrides)))
        .map_err(|st| format!("mount overrides status {st}"))?;
    eprintln!(
        "  composition: zip{} + overrides (no Steam-disk mount; under-root sealed to director)",
        if mods_dir.is_some() { " + mods" } else { "" }
    );

    // Prove steam_appid is visible through the director (what the shim FUSE sees).
    match session.kernel().getattr("steam_appid.txt") {
        Ok(Some(st)) => eprintln!(
            "  VFS ok: steam_appid.txt (size={}) — RestartAppIfNecessary override live",
            st.size
        ),
        Ok(None) => eprintln!("  warning: steam_appid.txt getattr → None (DRM may relaunch Steam)"),
        Err(e) => eprintln!("  warning: steam_appid.txt getattr err {e}"),
    }

    // Cheap sanity: getattr + 4-byte head only (do NOT full-read 250MB masters).
    for vpath in ["SkyrimSE.exe", "Data/Skyrim.esm"] {
        let st = session
            .kernel()
            .getattr(vpath)
            .map_err(|e| format!("getattr {vpath}: {e}"))?
            .ok_or_else(|| format!("VFS missing {vpath}"))?;
        let (fh, size, _) = session
            .kernel()
            .open(vpath, vfs_protocol::OPEN_READ)
            .map_err(|e| format!("open {vpath}: {e}"))?;
        let mut head = [0u8; 4];
        let n = session
            .kernel()
            .read(fh, 0, &mut head)
            .map_err(|e| format!("read {vpath}: {e}"))?;
        let _ = session.kernel().close(fh);
        eprintln!(
            "  VFS ok: {vpath} (meta size={} open size={size} head {:02x?})",
            st.size,
            &head[..n]
        );
    }

    session.serve().map_err(|e| format!("serve: {e}"))?;
    timeline.mark("serving");

    // ── stage the launch directory ─────────────────────────────────────────
    // CreateProcess needs a real on-disk image, and the loader resolves the
    // EXE's static imports before our shim exists. Export just that closure;
    // everything else stays virtual. `_staged` must outlive the child — its
    // Drop removes the directory, and the EXE is mapped while the game runs.
    let _staged;
    let host = match preset_host {
        Some(h) => h,
        None => {
            std::fs::create_dir_all(&stage_root)
                .map_err(|e| format!("mkdir {}: {e}", stage_root.display()))?;
            let swept = vfs_director::stage::sweep_stale(&stage_root);
            if swept > 0 {
                eprintln!("  reclaimed {swept} stale staging dir(s)");
            }
            // Launching via a loader (SKSE) stages the game beside it: the
            // loader spawns SkyrimSE.exe itself, and that CreateProcess needs a
            // real image just as ours did. SKSE also resolves its runtime DLL
            // relative to the loader, and that DLL is *not* a static import —
            // it is injected at runtime — so the closure walk cannot find it.
            let mut also_owned: Vec<String> = Vec::new();
            if !launch_exe.eq_ignore_ascii_case("SkyrimSE.exe") {
                also_owned.push("SkyrimSE.exe".to_string());
                if let Some(m) = &mods_dir {
                    for ent in std::fs::read_dir(m).into_iter().flatten().flatten() {
                        let n = ent.file_name().to_string_lossy().into_owned();
                        let l = n.to_ascii_lowercase();
                        if l.starts_with("skse64_") && l.ends_with(".dll") {
                            also_owned.push(n);
                        }
                    }
                }
            }
            let also: Vec<&str> = also_owned.iter().map(|s| s.as_str()).collect();
            let staged = vfs_director::stage::stage_launch_with(
                &KernelSource(&session),
                &launch_exe,
                &also,
                &stage_root,
                &std::process::id().to_string(),
                // DirectX redistributables are static imports but ship with the
                // DX runtime, not in the game archive.
                &[data.join("dx-redist")],
            )?;
            eprintln!(
                "  staged {} file(s) → {}: {}",
                staged.staged().len(),
                staged.dir().display(),
                staged.staged().join(", ")
            );
            let exe = staged.exe().to_string_lossy().into_owned();
            std::env::set_var("VFS_LAUNCH_IMAGE", &exe);
            _staged = staged;
            exe
        }
    };

    timeline.mark("staged");
    eprintln!("  IPC serving; launching the staged image…");
    eprintln!("  writes under the game root land in {}", overrides.display());
    eprintln!("  saves → {}", saves.display());
    eprintln!("  profiles/inis → {}", profiles.display());

    // Preflight host open/read is not ring I/O — reset so post-launch stats are clean.
    vfs_director::io_stats_reset();

    let wait = std::env::var("VFS_WAIT").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false);
    let code = session.launch(&LaunchOpts {
        image: host.clone(),
        args: vec![],
        wait,
        shim_dll: None,
        payload_dll: None,
        env: Default::default(),
    })?;

    timeline.mark("launched");
    vfs_director::io_mark_launch();
    eprintln!("  vfs-io: launch mark — counting child FUSE traffic from here");

    if bench {
        // Stop at the first rendered frame: that is the number a player feels,
        // and it bounds every cost on the path (staging, inject, streaming).
        use vfs_director::bench;
        let timeout = std::time::Duration::from_secs(300);
        let pid = bench::wait_for_pid("SkyrimSE.exe", timeout)
            .ok_or_else(|| "benchmark: SkyrimSE.exe never appeared".to_string())?;
        timeline.mark("game process");
        let size = bench::wait_for_window(pid, timeout);
        timeline.mark("window visible");

        let totals = vfs_director::io_stats::totals();
        let label = std::env::var("VFS_BENCH_LABEL").unwrap_or_else(|_| "run".to_string());
        eprint!("{}", bench::report(&timeline, &totals, &label));
        match size {
            Some((w, h)) => eprintln!("  window: {w}x{h} (pid {pid})"),
            None => eprintln!("  window: NONE within {}s — timing is a timeout, not a load", timeout.as_secs()),
        }
        eprintln!("\n{}", bench::markdown_header());
        eprintln!("{}", bench::markdown_row(&timeline, &totals, &label));

        // The staged dir is dropped when this returns, so stop the game first.
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/IM", "SkyrimSE.exe"])
            .output();
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/IM", "skse64_loader.exe"])
            .output();
        session.stop_serve();
    } else if wait {
        eprintln!("game exited with code {code}");
        eprint!("{}", vfs_director::io_stats_report(40));
        session.stop_serve();
    } else {
        eprintln!("game launched (detached). Keep this process alive for IPC — Ctrl+C to stop.");
        // Heartbeat + I/O dump so we can see whether BSAs/ESMs are actually read.
        let mut ticks = 0u32;
        let mut last_steam_ok = true;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            ticks = ticks.wrapping_add(1);
            let alive = game_process_alive();
            let steam_ok = steam_process_alive();
            if steam_ok != last_steam_ok {
                eprintln!(
                    "  steam client {} (DRM may break)",
                    if steam_ok { "still running" } else { "EXITED" }
                );
                last_steam_ok = steam_ok;
            }
            // Every 10s: full top-path I/O report.
            if ticks % 2 == 0 {
                eprint!("{}", vfs_director::io_stats_report(25));
            }
            if ticks % 6 == 0 {
                eprintln!(
                    "  ipc heartbeat t={}s game_alive={alive} steam_alive={steam_ok}",
                    ticks * 5
                );
            }
            if !alive && ticks >= 3 {
                eprintln!("  game process not found — final I/O dump:");
                eprint!("{}", vfs_director::io_stats_report(40));
                session.stop_serve();
                break;
            }
        }
    }
    Ok(())
}

/// If the zip has a single top-level directory, return its name.
fn detect_zip_root_prefix(be: &dyn Backend) -> Result<Option<String>, String> {
    let entries = be.readdir("").map_err(|e| format!("readdir root: {e}"))?;
    let dirs: Vec<_> = entries
        .iter()
        .filter(|e| e.stat.kind == KIND_DIR)
        .map(|e| e.name.clone())
        .collect();
    let files: Vec<_> = entries
        .iter()
        .filter(|e| e.stat.kind != KIND_DIR)
        .collect();
    if files.is_empty() && dirs.len() == 1 {
        Ok(Some(dirs[0].clone()))
    } else {
        Ok(None)
    }
}

/// Skyrim SE Steam AppID (appmanifest_489830.acf).
const SKYRIM_SE_APP_ID: &str = "489830";


/// Valve steam_appid.txt: lets SteamAPI_Init talk to the running client without
/// RestartAppIfNecessary → steam://run (Remote Play / UI relaunch).
fn write_steam_appid(root: &Path, overrides: &Path) -> Result<(), String> {
    let body = format!("{SKYRIM_SE_APP_ID}\n");
    // Physical next to host (anything that bypasses VFS early).
    let on_disk = root.join("steam_appid.txt");
    std::fs::write(&on_disk, &body).map_err(|e| format!("write {}: {e}", on_disk.display()))?;
    // Overlay so dual-layer/VFS open of <root>\steam_appid.txt sees it too.
    let in_overlay = overrides.join("steam_appid.txt");
    std::fs::write(&in_overlay, &body)
        .map_err(|e| format!("write {}: {e}", in_overlay.display()))?;
    eprintln!(
        "  steam_appid.txt → {} and {}",
        on_disk.display(),
        in_overlay.display()
    );
    Ok(())
}

/// Minimum Steam process age (seconds) before we allow launch.
/// CM connect often finishes well after webhelper appears; 20s was too short.
const STEAM_MIN_AGE_SECS: u64 = 45;

/// Pure gate used by [`ensure_steam_running`] (unit-tested).
fn steam_gate_decision(info: &SteamInfo) -> Result<(), String> {
    if !info.steam {
        return Err(
            "HARD REQUIREMENT: Steam is not running.\n\
             Start Steam, log in, wait until the library UI is idle, then re-run skyrim-live.\n\
             skyrim-live will never start Steam for you (that restarts the client and breaks DRM)."
                .into(),
        );
    }
    if !info.webhelper {
        return Err(
            "Steam.exe is present but steamwebhelper is not — client is still starting.\n\
             Wait until Steam is fully logged in (library usable), then re-run."
                .into(),
        );
    }
    if info.age_secs < STEAM_MIN_AGE_SECS {
        return Err(format!(
            "Steam only started {}s ago — wait until it is fully up (≥{STEAM_MIN_AGE_SECS}s, library idle), then re-run.\n\
             Launching into a half-started client causes DRM flakiness.",
            info.age_secs
        ));
    }
    Ok(())
}

/// **Hard requirement:** Steam must already be running and settled before launch.
///
/// Never spawn steam.exe from here. Starting the client mid-session restarts
/// the UI, tears down the DRM pipe, and is exactly the "Steam reopened /
/// Remote Play" failure mode. The game only *finds* the running client.
fn ensure_steam_running() -> Result<(), String> {
    let info = steam_client_info();
    steam_gate_decision(&info)?;
    eprintln!(
        "  steam client OK (pid age ~{}s, webhelper present) — DRM via IPC only; will NOT start/restart Steam",
        info.age_secs
    );
    Ok(())
}

/// Skyrim SE Steam AppID — used for overlay disable + log filters.
const SKYRIM_SE_APPID: &str = "489830";

/// Best-effort: turn off in-game overlay for Skyrim SE so Steam does not inject
/// `gameoverlayui64` into the game process (observed CM assert + Shutdown).
///
/// Writes `userdata/*/config/localconfig.vdf` and HKCU Apps\\489830. If Steam
/// was already running when the VDF changed, a client restart (by the user) may
/// be required for the client to re-read it — we still apply registry now.
fn disable_skyrim_game_overlay() -> Result<(), String> {
    #[cfg(windows)]
    {
        // Registry: per-app Overlay = 0 (and clear stuck Running flag).
        let _ = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    r#"
$path = 'HKCU:\Software\Valve\Steam\Apps\{SKYRIM_SE_APPID}'
if (-not (Test-Path $path)) {{ New-Item -Path $path -Force | Out-Null }}
New-ItemProperty -Path $path -Name Overlay -Value 0 -PropertyType DWord -Force | Out-Null
New-ItemProperty -Path $path -Name Running -Value 0 -PropertyType DWord -Force | Out-Null
# Global overlay off if present
$steam = 'HKCU:\Software\Valve\Steam'
if (Test-Path $steam) {{
  New-ItemProperty -Path $steam -Name InGameOverlay -Value 0 -PropertyType DWord -Force | Out-Null
}}
'reg-ok'
"#
                ),
            ])
            .output();

        // localconfig.vdf: inject EnableGameOverlay "0" into the 489830 apps block.
        let userdata = Path::new(r"C:\Program Files (x86)\Steam\userdata");
        if userdata.is_dir() {
            for entry in std::fs::read_dir(userdata).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let lc = entry.path().join("config").join("localconfig.vdf");
                if !lc.is_file() {
                    continue;
                }
                match inject_enable_game_overlay_off(&lc, SKYRIM_SE_APPID) {
                    Ok(true) => eprintln!(
                        "  overlay: set EnableGameOverlay=0 for app {SKYRIM_SE_APPID} in {}",
                        lc.display()
                    ),
                    Ok(false) => eprintln!(
                        "  overlay: localconfig already has EnableGameOverlay=0 ({})",
                        lc.display()
                    ),
                    Err(e) => eprintln!("  overlay: localconfig patch skipped: {e}"),
                }
            }
        }
        eprintln!(
            "  overlay: disabled for app {SKYRIM_SE_APPID} (registry+localconfig); restart Steam once if it was already running so the client reloads VDF"
        );
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

/// Insert or force `EnableGameOverlay "0"` inside the `"appid" { ... }` block.
/// Returns Ok(true) if the file was modified.
fn inject_enable_game_overlay_off(path: &Path, appid: &str) -> Result<bool, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    // Already off for this app?
    let marker = format!("\"{appid}\"");
    let Some(app_pos) = raw.find(&marker) else {
        return Ok(false);
    };
    // Find the opening brace after the appid key.
    let after = &raw[app_pos + marker.len()..];
    let Some(brace_rel) = after.find('{') else {
        return Ok(false);
    };
    let block_start = app_pos + marker.len() + brace_rel;
    // Naive brace match for this app block (VDF is nested but we only scan text).
    let mut depth = 0i32;
    let mut block_end = None;
    for (i, ch) in raw[block_start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    block_end = Some(block_start + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let block_end = block_end.ok_or_else(|| "unclosed app block".to_string())?;
    let block = &raw[block_start..=block_end];
    if block.contains("\"EnableGameOverlay\"") {
        // Force to 0 if present as 1.
        if block.contains("\"EnableGameOverlay\"\t\t\"0\"")
            || block.contains("\"EnableGameOverlay\" \"0\"")
            || block.contains("\"EnableGameOverlay\"\t\"0\"")
        {
            return Ok(false);
        }
        let patched_block = block
            .replace("\"EnableGameOverlay\"\t\t\"1\"", "\"EnableGameOverlay\"\t\t\"0\"")
            .replace("\"EnableGameOverlay\" \"1\"", "\"EnableGameOverlay\" \"0\"")
            .replace("\"EnableGameOverlay\"\t\"1\"", "\"EnableGameOverlay\"\t\"0\"");
        if patched_block == block {
            return Ok(false);
        }
        let mut out = String::with_capacity(raw.len());
        out.push_str(&raw[..block_start]);
        out.push_str(&patched_block);
        out.push_str(&raw[block_end + 1..]);
        std::fs::write(path, out).map_err(|e| format!("write {}: {e}", path.display()))?;
        return Ok(true);
    }
    // Insert right after the opening brace.
    let insert_at = block_start + 1;
    let insertion = "\n\t\t\"EnableGameOverlay\"\t\t\"0\"";
    let mut out = String::with_capacity(raw.len() + insertion.len());
    out.push_str(&raw[..insert_at]);
    out.push_str(insertion);
    out.push_str(&raw[insert_at..]);
    std::fs::write(path, out).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(true)
}

/// Wait until Steam is usable for DRM IPC: CM **Connected**, or **offline mode**.
///
/// Launching while CM is mid-connect correlates with
/// `LogonFailure No Connection` + client Shutdown. Offline is preferred when
/// multi-session kicks (`Logged In Elsewhere`) kill the desktop client on game adopt.
fn ensure_steam_logged_on() -> Result<(), String> {
    #[cfg(windows)]
    {
        let log = Path::new(r"C:\Program Files (x86)\Steam\logs\connection_log.txt");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        let mut last_state = String::from("unknown");
        loop {
            if steam_offline_mode_active() {
                eprintln!(
                    "  steam offline mode active — skipping CM Connected wait (local DRM IPC only)"
                );
                return Ok(());
            }
            match steam_cm_state(log) {
                Ok(state) if state.eq_ignore_ascii_case("Connected") => {
                    eprintln!("  steam CM state: Connected (logged on) — safe to launch");
                    return Ok(());
                }
                Ok(state) => {
                    last_state = state;
                    eprintln!("  steam CM state: {last_state} — waiting for Connected…");
                }
                Err(e) => {
                    last_state = e;
                    eprintln!("  steam CM probe: {last_state}");
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "Steam is running but not fully logged on (CM state={last_state}).\n\
                     Wait until the Steam library shows you online, OR start Steam offline \
                     (`steam.exe -offline`) to avoid multi-session kicks, then re-run.\n\
                     Hollow launch before CM Connected causes LogonFailure / client Shutdown."
                ));
            }
            if !steam_process_alive() {
                return Err(
                    "Steam exited while waiting for CM Connected. Start Steam, log in, re-run."
                        .into(),
                );
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

/// True when Steam is configured/running offline (no live CM session required).
fn steam_offline_mode_active() -> bool {
    #[cfg(windows)]
    {
        // loginusers.vdf: WantsOfflineMode "1"
        let lu = Path::new(r"C:\Program Files (x86)\Steam\config\loginusers.vdf");
        if let Ok(raw) = std::fs::read_to_string(lu) {
            if raw.contains("\"WantsOfflineMode\"") {
                // Accept tab/space variants of "1"
                for line in raw.lines() {
                    if line.contains("WantsOfflineMode") && line.contains('1') {
                        return true;
                    }
                }
            }
        }
        // connection_log: recent Offline / no network path
        let log = Path::new(r"C:\Program Files (x86)\Steam\logs\connection_log.txt");
        if let Ok(raw) = std::fs::read(log) {
            let start = raw.len().saturating_sub(32 * 1024);
            let text = String::from_utf8_lossy(&raw[start..]);
            if text.to_ascii_lowercase().contains("offline")
                && !text.contains("[Connected")
            {
                return true;
            }
        }
        // console_log tail
        let console = Path::new(r"C:\Program Files (x86)\Steam\logs\console_log.txt");
        if let Ok(raw) = std::fs::read(console) {
            let start = raw.len().saturating_sub(16 * 1024);
            let text = String::from_utf8_lossy(&raw[start..]);
            if text.contains("Offline mode") || text.contains("Steam is offline") {
                return true;
            }
        }
        false
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Parse the *last* connection state token from Steam's connection_log.txt.
///
/// Lines look like: `[Connected, 0, 11] [U:1:…] …` or `[Logged Off, 0, 0] …`
fn steam_cm_state(log: &Path) -> Result<String, String> {
    if !log.is_file() {
        return Err("connection_log.txt missing".into());
    }
    let raw = std::fs::read(log).map_err(|e| format!("read connection_log: {e}"))?;
    // File can be large; only scan the last ~64 KiB.
    let start = raw.len().saturating_sub(64 * 1024);
    let text = String::from_utf8_lossy(&raw[start..]);
    let mut last = None;
    for line in text.lines() {
        // Prefer bracket state at start of payload after timestamp.
        // Format: [YYYY-MM-DD HH:MM:SS] [Connected, …]
        if let Some(idx) = line.find("[Connected") {
            if line[idx..].starts_with("[Connected") {
                last = Some("Connected");
            }
        } else if line.contains("[Logged Off") {
            last = Some("Logged Off");
        } else if line.contains("[Connecting") {
            last = Some("Connecting");
        } else if line.contains("[Disconnected") {
            last = Some("Disconnected");
        }
    }
    last.map(|s| s.to_string())
        .ok_or_else(|| "no CM state tokens in connection_log tail".into())
}

#[cfg(test)]
mod steam_gate_tests {
    use super::*;

    #[test]
    fn refuses_when_steam_absent() {
        let e = steam_gate_decision(&SteamInfo {
            steam: false,
            webhelper: false,
            age_secs: 0,
        })
        .unwrap_err();
        assert!(e.contains("HARD REQUIREMENT"), "{e}");
        assert!(e.contains("never start Steam"), "{e}");
    }

    #[test]
    fn refuses_when_webhelper_missing() {
        let e = steam_gate_decision(&SteamInfo {
            steam: true,
            webhelper: false,
            age_secs: 60,
        })
        .unwrap_err();
        assert!(e.contains("webhelper"), "{e}");
    }

    #[test]
    fn refuses_when_too_young() {
        let e = steam_gate_decision(&SteamInfo {
            steam: true,
            webhelper: true,
            age_secs: 5,
        })
        .unwrap_err();
        assert!(e.contains("5s"), "{e}");
    }

    #[test]
    fn accepts_settled_client() {
        steam_gate_decision(&SteamInfo {
            steam: true,
            webhelper: true,
            age_secs: 60,
        })
        .unwrap();
    }

    #[test]
    fn inject_overlay_off_inserts_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!(
            "vfs-overlay-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("localconfig.vdf");
        let body = r#""UserLocalConfigStore"
{
	"Software"
	{
		"Valve"
		{
			"Steam"
			{
				"apps"
				{
					"489830"
					{
						"LastPlayed"		"1"
						"Playtime"		"2"
					}
				}
			}
		}
	}
}
"#;
        std::fs::write(&path, body).unwrap();
        assert!(inject_enable_game_overlay_off(&path, "489830").unwrap());
        let once = std::fs::read_to_string(&path).unwrap();
        assert!(once.contains("EnableGameOverlay"), "{once}");
        assert!(once.contains("\"0\""), "{once}");
        assert!(!inject_enable_game_overlay_off(&path, "489830").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

struct SteamInfo {
    steam: bool,
    webhelper: bool,
    age_secs: u64,
}

fn steam_client_info() -> SteamInfo {
    #[cfg(windows)]
    {
        let ps = r#"
$s = Get-Process -Name steam -ErrorAction SilentlyContinue | Select-Object -First 1
$w = Get-Process -Name steamwebhelper -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $s) { 'none'; exit 0 }
$age = [int]((Get-Date) - $s.StartTime).TotalSeconds
$wh = if ($w) { '1' } else { '0' }
Write-Output ("ok|$age|$wh")
"#;
        if let Ok(o) = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", ps])
            .output()
        {
            let s = String::from_utf8_lossy(&o.stdout);
            let line = s.lines().last().unwrap_or("").trim();
            if line.starts_with("ok|") {
                let parts: Vec<&str> = line.split('|').collect();
                let age = parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(0);
                let wh = parts.get(2).map(|x| *x == "1").unwrap_or(false);
                return SteamInfo {
                    steam: true,
                    webhelper: wh,
                    age_secs: age,
                };
            }
        }
        // Fallback: tasklist presence only (age unknown → treat as settled if present).
        let steam = steam_process_alive();
        SteamInfo {
            steam,
            webhelper: steam,
            age_secs: if steam { 60 } else { 0 },
        }
    }
    #[cfg(not(windows))]
    {
        SteamInfo {
            steam: false,
            webhelper: false,
            age_secs: 0,
        }
    }
}

fn steam_process_alive() -> bool {
    #[cfg(windows)]
    {
        if let Ok(o) = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "if (Get-Process -Name steam -ErrorAction SilentlyContinue) { 'yes' }",
            ])
            .output()
        {
            let s = String::from_utf8_lossy(&o.stdout);
            if s.to_ascii_lowercase().contains("yes") {
                return true;
            }
        }
        std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq steam.exe", "/NH"])
            .output()
            .map(|o| {
                let s = String::from_utf8_lossy(&o.stdout);
                s.to_ascii_lowercase().contains("steam.exe")
            })
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Only wipe throwaway managed roots, never the Steam library tree.
fn is_safe_to_wipe(root: &Path) -> bool {
    let s = root.to_string_lossy().to_ascii_lowercase();
    s.contains(r"\tmp\")
        || s.contains("/tmp/")
        || s.contains("skyrim-runtime")
        || s.contains("skyrim-data")
        || s.contains(r"\temp\")
}

fn game_process_alive() -> bool {
    #[cfg(windows)]
    {
        // Lightweight check: tasklist would be slow; use CreateToolhelp32 snapshot via cmd.
        // Prefer matching image name SkyrimSE.exe.
        std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq SkyrimSE.exe", "/NH"])
            .output()
            .map(|o| {
                let s = String::from_utf8_lossy(&o.stdout);
                s.to_ascii_lowercase().contains("skyrimse.exe")
            })
            .unwrap_or(true)
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// Copy legacy DX DLLs into `root` (and ensure a durable cache under `cache`).
fn stage_dx_redist(root: &Path, cache: &Path) -> Result<(), String> {
    std::fs::create_dir_all(cache).map_err(|e| e.to_string())?;
    // Skyrim SE imports d3dx9_42 + X3DAudio1_7 + XInput1_3 among others.
    let names = [
        "X3DAudio1_7.dll",
        "XAudio2_7.dll",
        "XAPOFX1_5.dll",
        "xinput1_3.dll",
        "xinput1_4.dll",
        "D3DX9_42.dll",
        "D3DX9_43.dll",
        "d3dx11_43.dll",
        "D3DCompiler_43.dll",
        "d3dx10_43.dll",
    ];
    // Prefer already-cached copies; else pull from the winget MSIX package.
    let pkg = r"C:\Program Files\WindowsApps\Microsoft.DirectXRuntime_9.29.1974.0_x64__8wekyb3d8bbwe";
    let mut n = 0usize;
    for name in names {
        let dest_cache = cache.join(name);
        if !dest_cache.is_file() {
            let src = Path::new(pkg).join(name);
            // WindowsApps names are mixed-case; try case-insensitive scan.
            let src = if src.is_file() {
                src
            } else {
                match std::fs::read_dir(pkg) {
                    Ok(rd) => rd
                        .flatten()
                        .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name))
                        .map(|e| e.path())
                        .unwrap_or(src),
                    Err(_) => src,
                }
            };
            if src.is_file() {
                let _ = std::fs::copy(&src, &dest_cache);
            }
        }
        if dest_cache.is_file() {
            let dest = root.join(name);
            let _ = std::fs::copy(&dest_cache, &dest);
            n += 1;
        }
    }
    if n > 0 {
        eprintln!("  staged {n} DirectX runtime DLL(s) under {}", root.display());
    } else {
        eprintln!(
            "  warning: no DirectX runtime DLLs found — install Microsoft.DirectX \
             (winget) or place X3DAudio1_7.dll under {}",
            cache.display()
        );
    }
    Ok(())
}

fn wipe_files(root: &Path) -> Result<(), String> {
    fn walk(dir: &Path) -> Result<usize, String> {
        let mut n = 0;
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return Ok(0),
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                n += walk(&p)?;
            } else if p.is_file() {
                std::fs::remove_file(&p).map_err(|e| format!("rm {}: {e}", p.display()))?;
                n += 1;
            }
        }
        Ok(n)
    }
    let n = walk(root)?;
    if n > 0 {
        eprintln!("  wiped {n} file(s) under {}", root.display());
    }
    Ok(())
}

fn setup_my_games_junctions(profiles: &Path, saves: &Path) -> Result<(), String> {
    let docs = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .ok_or_else(|| "USERPROFILE unset".to_string())?
        .join("Documents")
        .join("My Games")
        .join("Skyrim Special Edition");

    // Ensure nested Saves junction target exists under profiles before swapping.
    let profiles_saves = profiles.join("Saves");

    // Migrate existing real Saves → c:\tmp\skyrim-data\saves
    let old_saves = docs.join("Saves");
    if old_saves.is_dir() && !is_reparse_point(&old_saves) {
        eprintln!("  migrating existing Saves → {}", saves.display());
        merge_dir(&old_saves, saves)?;
        std::fs::remove_dir_all(&old_saves)
            .map_err(|e| format!("remove old Saves: {e}"))?;
    }

    // Migrate other My Games content into profiles (if real directory).
    if docs.is_dir() && !is_reparse_point(&docs) {
        eprintln!("  migrating My Games SSE → {}", profiles.display());
        for ent in std::fs::read_dir(&docs).map_err(|e| e.to_string())?.flatten() {
            let name = ent.file_name();
            if name == "Saves" {
                continue;
            }
            let dest = profiles.join(&name);
            let src = ent.path();
            if src.is_dir() {
                merge_dir(&src, &dest)?;
                let _ = std::fs::remove_dir_all(&src);
            } else if src.is_file() {
                let _ = std::fs::copy(&src, &dest);
                let _ = std::fs::remove_file(&src);
            }
        }
        // Remove empty shell if possible.
        let _ = std::fs::remove_dir_all(&docs);
    }

    // profiles\Saves → saves
    if profiles_saves.exists() && !is_reparse_point(&profiles_saves) {
        merge_dir(&profiles_saves, saves)?;
        let _ = std::fs::remove_dir_all(&profiles_saves);
    }
    ensure_junction(&profiles_saves, saves)?;

    // My Games\Skyrim Special Edition → profiles
    if let Some(parent) = docs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    ensure_junction(&docs, profiles)?;
    eprintln!(
        "  junction: {}  ⇒  {}",
        docs.display(),
        profiles.display()
    );
    eprintln!(
        "  junction: {}  ⇒  {}",
        profiles_saves.display(),
        saves.display()
    );
    Ok(())
}

fn setup_localappdata_junction(profiles: &Path) -> Result<(), String> {
    let local_sse = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "LOCALAPPDATA unset".to_string())?
        .join("Skyrim Special Edition");
    let target = profiles.join("LocalAppData");
    std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
    if local_sse.is_dir() && !is_reparse_point(&local_sse) {
        merge_dir(&local_sse, &target)?;
        let _ = std::fs::remove_dir_all(&local_sse);
    }
    ensure_junction(&local_sse, &target)?;
    eprintln!(
        "  junction: {}  ⇒  {}",
        local_sse.display(),
        target.display()
    );
    Ok(())
}

fn merge_dir(src: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    for ent in std::fs::read_dir(src).map_err(|e| e.to_string())?.flatten() {
        let from = ent.path();
        let to = dest.join(ent.file_name());
        if from.is_dir() {
            merge_dir(&from, &to)?;
        } else if from.is_file() && !to.exists() {
            std::fs::copy(&from, &to).map_err(|e| format!("copy {}: {e}", from.display()))?;
        }
    }
    Ok(())
}

fn is_reparse_point(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    // symlink_metadata does not follow junctions/symlinks.
    std::fs::symlink_metadata(path)
        .map(|m| m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
        .unwrap_or(false)
}

fn ensure_junction(link: &Path, target: &Path) -> Result<(), String> {
    if link.exists() || is_reparse_point(link) {
        if is_reparse_point(link) {
            // Already a reparse point — leave it (assume correct).
            return Ok(());
        }
        // Real path still there.
        return Err(format!(
            "cannot create junction {}; path exists and is not a junction",
            link.display()
        ));
    }
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // cmd mklink /J
    let status = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &link.to_string_lossy(),
            &target.to_string_lossy(),
        ])
        .status()
        .map_err(|e| format!("mklink: {e}"))?;
    if !status.success() {
        return Err(format!(
            "mklink /J {} {} failed: {status}",
            link.display(),
            target.display()
        ));
    }
    Ok(())
}
