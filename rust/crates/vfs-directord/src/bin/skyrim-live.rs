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
//!
//! **Two managed roots** (stage 2b task 6): root 0 is the game directory
//! above; root 1 is `Documents\My Games\Skyrim Special Edition` — the
//! junction `setup_my_games_junctions` points at `profiles`, declared here so
//! the gate-1 baseline's headline open question (does the game's save route
//! through the director, or does it still bypass every counter through that
//! junction?) can actually be measured instead of assumed. See
//! `resolve_second_root_target`'s doc comment for the junction-resolution
//! detail and `print_open_totals`/`CountingProvider` for the per-root
//! counters this adds.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use vfs_compose::SubdirProvider;
use vfs_director::{DiskProvider, LaunchOpts, Provider, RootId, Session, OPEN_WRITE};
use vfs_protocol::{Capabilities, DirEntry, Handle, SetAttr, Stat, VPath, KIND_DIR};
use vfs_zip::ZipProvider;

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
        let (fh, size, _) = k.open(RootId::DEFAULT, vpath, vfs_protocol::OPEN_READ).ok()?;
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
    let bench = vfs_env::opt_in(vfs_env::BENCH);

    let zip_path = env_path(vfs_env::SKYRIM_ZIP, r"C:\tmp\skyrimse.zip");
    let data = env_path(vfs_env::SKYRIM_DATA, r"C:\tmp\skyrim-data");
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
    let preset_host = vfs_env::text(vfs_env::LAUNCH_IMAGE).filter(|h| Path::new(h).is_file());

    // Managed root: never the Steam tree. Content is served here from the zip.
    let root = if let Some(r) = vfs_env::raw(vfs_env::SKYRIM_ROOT) {
        PathBuf::from(r)
    } else {
        PathBuf::from(r"C:\tmp\skyrim-runtime")
    };
    let stage_root = data.join("stage");
    // Fixed per process run, computed up front so the disk provider mounted
    // over the staging directory below (before staging has even happened)
    // and the later `stage_launch_with` call agree on exactly the same path.
    let stage_tag = std::process::id().to_string();
    let staged_dir_path =
        stage_root.join(format!("{}{stage_tag}", vfs_director::stage::STAGE_PREFIX));

    // Optional mod overlay (e.g. an unpacked SKSE) composed over the zip, and
    // the executable to launch — `skse64_loader.exe` to go through SKSE.
    let mods_dir = vfs_env::path(vfs_env::SKYRIM_MODS);
    let launch_exe =
        vfs_env::text(vfs_env::SKYRIM_LAUNCH).unwrap_or_else(|| "SkyrimSE.exe".to_string());

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
    for d in [&state, &saves, &profiles, &overrides, &root] {
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
    let my_games_docs = setup_my_games_junctions(&profiles, &saves)?;
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
    if !vfs_env::present(vfs_env::KEEP_HOST_STEAM_API) {
        std::env::set_var(vfs_env::KEEP_HOST_STEAM_API, "1");
    }
    std::env::remove_var(vfs_env::ALLOW_DISK_FALLTHROUGH);
    std::env::remove_var(vfs_env::DISK_ONLY_ROOT);
    if let Some(p) = vfs_env::raw(vfs_env::DIRECTOR_OPEN_LOG) {
        eprintln!("  director-open log: {}", p.to_string_lossy());
    }

    // ── base content: a directory tree, or the zip ──────────────────────────
    // `VFS_SKYRIM_DISK` swaps the archive for an already-extracted install. The
    // point is differential diagnosis: it keeps the shim, the staging, the
    // sealed root and the launch path identical while changing only where bytes
    // come from, so a behaviour that survives the swap is not the archive's.
    let disk_src = vfs_env::path(vfs_env::SKYRIM_DISK);
    let backend: Arc<dyn Provider> = if let Some(d) = &disk_src {
        if !d.is_dir() {
            return Err(format!("VFS_SKYRIM_DISK not a directory: {}", d.display()));
        }
        eprintln!("  base:      {}  (disk tree, zip bypassed)", d.display());
        timeline.mark("zip index");
        Arc::new(DiskProvider::new(d))
    } else {
        // Open the zip ONCE. (A second open re-scans the 16GB CD and looks frozen.)
        eprintln!("  opening zip index (one-time CD parse; may take ~30–90s on a 16GB archive)…");
        let t0 = std::time::Instant::now();
        let zip = ZipProvider::open(&zip_path).map_err(|e| format!("ZipProvider: {e:?}"))?;
        eprintln!("  zip index ready in {:.1}s", t0.elapsed().as_secs_f32());
        timeline.mark("zip index");

        // Detect single top-level folder from the already-open backend (no re-open).
        let prefix = detect_zip_root_prefix(&zip)?;
        eprintln!("  zip root prefix: {prefix:?}");
        if let Some(pfx) = prefix {
            Arc::new(SubdirProvider::new(Arc::new(zip), pfx))
        } else {
            Arc::new(zip)
        }
    };

    let mut session = Session::new();
    session.set_root(&root);
    session.set_overlay(&overrides);
    session.set_state_dir(&state);

    mount_low_priority_disk_layers(&session, &root, &staged_dir_path)?;

    // Composition: zip (game content) under overrides (steam_appid, writes).
    // Do **not** mount the Steam library DiskProvider — that let the game load
    // masters/BSAs/DLLs from the host install and violated the VFS contract.
    // steam_appid.txt is written into the overrides layer before launch.
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
            .mount("", Arc::new(DiskProvider::new(m)))
            .map_err(|st| format!("mount mods status {st}"))?;
    }
    // The write layer — an overlay **upper** over everything mounted above,
    // not one more sibling mount (gate 4, Task 6).
    //
    // It addresses the root-0 subdirectory the shim's local write overlay
    // also uses (`Session::overlay_layer_dir`), not `overrides` itself: the
    // overlay is root-scoped on disk (gate 4, Task 2), so pointing at the
    // bare `overrides` directory here would show the director an empty layer
    // while every write the shim makes lands one level deeper, under
    // `overrides/root-0/` — a write→read-back round trip through the
    // director would silently see nothing back. `write_steam_appid` already
    // created this subdirectory (and seeded `steam_appid.txt` into it), so it
    // exists by the time this is declared.
    //
    // **Why an upper and not a mount.** As a sibling mount this directory
    // could only *receive* writes the graph routed to it; it could not seed
    // one from the zip first. So an in-place edit of zip content
    // (`fopen(..., "r+b")`) found no writable mount holding the file, reached
    // the zip, and was refused `ST_READ_ONLY` — which the shim's write
    // fall-through used to paper over by diverting the write into its own
    // local overlay, and, once Task 5 sealed that, surfaced as a hard
    // `STATUS_ACCESS_DENIED` to the game. Copy-on-write over read-only
    // layered content is what a mod-manager VFS is *for*, so it belongs in
    // the provider graph: `Session::set_write_layer` composes an
    // `OverlayProvider` whose base is the whole mount graph and whose upper
    // is this directory, and the director does the copy-up itself.
    let overrides_root0 = session.overlay_layer_dir(RootId::DEFAULT);
    session
        .set_write_layer(Arc::new(DiskProvider::new(&overrides_root0)))
        .map_err(|st| format!("set write layer status {st}"))?;
    eprintln!(
        "  composition: zip{} under a copy-on-write overlay whose upper is {} \
(no Steam-disk mount; under-root sealed to director)",
        if mods_dir.is_some() { " + mods" } else { "" },
        overrides_root0.display()
    );

    // ── second managed root: Documents\My Games\Skyrim Special Edition ─────
    // Gate 1's baseline found the game's own save invisible to every
    // counter: it travels through the junction `setup_my_games_junctions`
    // just created (`my_games_docs` -> `profiles`), which sits outside root
    // 0 (`root`) entirely, so neither the shim's classifier nor the
    // director's reconciliation ever saw it as anything but "outside-root".
    // Declaring it as root 1 is this task's whole question: does the save
    // now route through the director, or does it still bypass? Either
    // answer is the deliverable — see `resolve_second_root_target`'s doc
    // comment for why the path handed to `declare_root` (the junction's own
    // spelling) and the path backing this root's provider (its resolved
    // target) are deliberately different strings, not an oversight.
    let profiles_target = resolve_second_root_target(&my_games_docs)?;
    eprintln!(
        "  root 1:    {}  (Documents\\My Games junction; declared as-is so the shim's literal-\
component match sees exactly what the game's own raw NT open spells)",
        my_games_docs.display()
    );
    eprintln!(
        "  root 1 resolves via GetFinalPathNameByHandleW → {}",
        profiles_target.display()
    );
    if resolved_target_matches(&profiles_target, &profiles) {
        eprintln!("  root 1 resolution OK: matches the configured profiles dir ({})", profiles.display());
    } else {
        eprintln!(
            "  WARNING: root 1's resolved target does not match the configured profiles dir \
({}) — verify the junction (`mklink /J` output above) before trusting this run's root-1 numbers",
            profiles.display()
        );
    }
    // A single `DiskProvider` covering the whole of `profiles_target` is the
    // right shape for this root's *content* (unlike the root-0 mistake a
    // prior review flagged — `DiskProvider::new(root)` mounted at `/`
    // alongside real content, which made *everything* trivially "route" for
    // an uninteresting reason): this root has exactly one content source, so
    // one provider covering it is the whole composition, not a shortcut that
    // hides a negative result. If the game's save never actually opens
    // anything under `my_games_docs`'s own spelling (e.g. because it
    // resolves the Documents folder differently on some other machine), this
    // root's counters read zero — a real negative, not something this
    // mount shape can paper over.
    //
    // **Root 1's shim-overlay layer is mounted too** (gate 4, Task 6). Until
    // now `Session::mount`/`set_write_layer` composed a layer over the shim's
    // write overlay for root 0 *only*, so anything the shim was forced to
    // write locally under root 1 (`<overrides>/root-1/…`) was invisible to
    // the director: written, then read back as missing. That is dormant while
    // root 1's own provider is a `ReadWrite` `DiskProvider` — the director
    // takes every root-1 write, so the shim never reaches its overlay — and
    // goes live the moment root 1 becomes read-only. Mounting it here rather
    // than leaving it for that day is the same reasoning `declare_root`
    // carries: a layer that is missing fails silently.
    //
    // Composed with the shim's overlay as the **base** and the real profiles
    // directory as the writable **upper**, which is the opposite arrangement
    // from root 0 and deliberate: root 0's write layer and the shim's overlay
    // are the same physical directory, so there is nothing to choose between;
    // root 1's are different directories, and saves must keep landing in
    // `profiles` (that is what the junction measurement is about). As the
    // base, the shim's overlay is visible to reads and is copied up into
    // `profiles` on first write, instead of diverting the save away from the
    // directory this harness exists to observe.
    //
    // Composed by [`vfs_director::compose_root`] — the same function
    // `Session::recompose` and the daemon's `SessionRegistry` use — rather
    // than by assembling an `OverlayProvider` here. It cannot go through
    // `Session` itself (the counters have to wrap the *composed* provider and
    // `Session` composes internally, with no hook for a wrapper), but it can
    // and must use the same composition: this harness is the only route with
    // live evidence behind it, so it is the last route that should be off the
    // shared path. What `compose_root` returns here differs from the previous
    // hand-built overlay only by a transparent single-mount `MountGraph`
    // around the base.
    let root1_shim_overlay = vfs_director::overlay_layer_dir(&overrides, RootId(1));
    std::fs::create_dir_all(&root1_shim_overlay)
        .map_err(|e| format!("mkdir {}: {e}", root1_shim_overlay.display()))?;
    let root1_composed = vfs_director::compose_root(
        vec![(
            String::new(),
            Arc::new(DiskProvider::new(&root1_shim_overlay)) as Arc<dyn Provider>,
        )],
        Some(Arc::new(DiskProvider::new(&profiles_target))),
    )
    .map_err(|st| format!("compose root1: status {st}"))?;
    // The counters wrap the whole composed root, not just its writable half,
    // so `print_open_totals` still reports everything root 1 answers.
    let root1_counters = Arc::new(CountingProvider::new(root1_composed));
    session
        .kernel()
        .mount(RootId(1), Arc::clone(&root1_counters) as Arc<dyn Provider>)
        .map_err(|st| format!("mount root1 status {st}"))?;
    eprintln!(
        "  root 1 write overlay layer: {} (shim-local writes under root 1 stay visible to the director)",
        root1_shim_overlay.display()
    );
    session.declare_root(1, my_games_docs.clone());

    // Prove steam_appid is visible through the director (what the shim FUSE sees).
    match session.kernel().getattr(RootId::DEFAULT, "steam_appid.txt") {
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
            .getattr(RootId::DEFAULT, vpath)
            .map_err(|e| format!("getattr {vpath}: {e}"))?
            .ok_or_else(|| format!("VFS missing {vpath}"))?;
        let (fh, size, _) = session
            .kernel()
            .open(RootId::DEFAULT, vpath, vfs_protocol::OPEN_READ)
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
                &stage_tag,
                // DirectX redistributables are static imports but ship with the
                // DX runtime, not in the game archive.
                &[data.join("dx-redist")],
            )?;
            debug_assert_eq!(
                staged.dir(),
                staged_dir_path.as_path(),
                "stage_launch_with must land exactly where the staging disk provider was mounted"
            );
            eprintln!(
                "  staged {} file(s) → {}: {}",
                staged.staged().len(),
                staged.dir().display(),
                staged.staged().join(", ")
            );
            let exe = staged.exe().to_string_lossy().into_owned();
            std::env::set_var(vfs_env::LAUNCH_IMAGE, &exe);
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

    let wait = vfs_env::opt_in(vfs_env::WAIT);
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
        let label = vfs_env::text(vfs_env::BENCH_LABEL).unwrap_or_else(|| "run".to_string());
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
        print_open_totals(&root1_counters);
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
            if ticks.is_multiple_of(2) {
                eprint!("{}", vfs_director::io_stats_report(25));
                print_open_totals(&root1_counters);
            }
            if ticks.is_multiple_of(6) {
                eprintln!(
                    "  ipc heartbeat t={}s game_alive={alive} steam_alive={steam_ok}",
                    ticks * 5
                );
            }
            if !alive && ticks >= 3 {
                eprintln!("  game process not found — final I/O dump:");
                eprint!("{}", vfs_director::io_stats_report(40));
                print_open_totals(&root1_counters);
                session.stop_serve();
                break;
            }
        }
    }
    Ok(())
}

/// Mount two additive, lowest-priority layers, before any real content is
/// mounted, so a same-named real content file always wins:
///
/// - `root` itself: `stage_dx_redist` copies DX runtime DLLs straight onto
///   disk there (the loader's CWD-based DLL search needs them physically
///   present before hooks exist — see that function's doc comment), and
///   `write_steam_appid` drops a raw `steam_appid.txt` copy there too. Once
///   the managed root goes fully virtual, only the provider graph gets
///   asked — mounting `root` itself as a disk provider is this project's own
///   decided answer for "want a real directory's contents visible? mount a
///   disk provider", applied to every such file at once rather than one at a
///   time.
/// - `staged_dir_path`: where the launch EXE (and its import closure) will
///   land once staged, mounted at its final path *before* anything is
///   staged there. `DiskProvider` reads lazily, so this is safe as long as
///   nothing queries it before staging actually runs — true here, since the
///   caller mounts this immediately after `Session::new()`, well before
///   `stage_launch_with` populates the directory later in `run()`.
fn mount_low_priority_disk_layers(
    session: &Session,
    root: &Path,
    staged_dir_path: &Path,
) -> Result<(), String> {
    session
        .mount("", Arc::new(DiskProvider::new(root)))
        .map_err(|st| format!("mount root-disk status {st}"))?;
    session
        .mount("", Arc::new(DiskProvider::new(staged_dir_path)))
        .map_err(|st| format!("mount staging-disk status {st}"))?;
    Ok(())
}

/// Resolve `docs` (the `Documents\My Games\Skyrim Special Edition` junction
/// `setup_my_games_junctions` just created) to the real directory it
/// currently points at, via `GetFinalPathNameByHandleW`
/// (`vfs_win::final_path_for_open`) — the same authoritative, OS-consulted
/// resolution `vfs-redirect` itself uses for a junction/8.3/subst spelling,
/// rather than trusting that `profiles` (this project's own configured
/// target) is necessarily still what the junction resolves to.
///
/// **Deliberately not used for `declare_root`.** `Session::declare_root`'s
/// path is matched *literally*, component-by-component, against whatever a
/// real NT open spells (`RootMap::match_canonical`) — it is never itself
/// resolved through a junction (see `vfs-redirect`'s
/// `root_itself_being_a_reparse_point_is_never_aliased`, which proves a
/// declared root's own path is excluded from the junction-alias scan on
/// purpose). The gate-1 baseline's own captured log shows the game's raw
/// save open spelled exactly as the junction's own path
/// (`\??\c:\users\...\documents\my games\skyrim special edition\saves\...`),
/// never the resolved target — so declaring root 1 with the *resolved*
/// path here would build a `RootMap` entry the shim's literal match can
/// never satisfy, silently leaving the save exactly as invisible as before.
/// What resolution *is* for: the **provider** backing root 1 needs a real
/// directory, and asking the OS once, explicitly and verifiably, is more
/// honest than assuming `profiles` is still correct or relying a second time
/// on the disk layer's own transparent junction-following.
fn resolve_second_root_target(docs: &Path) -> Result<PathBuf, String> {
    let raw = docs.to_string_lossy().into_owned();
    let resolved = vfs_win::final_path_for_open(&raw).ok_or_else(|| {
        format!(
            "could not resolve {} via GetFinalPathNameByHandleW (does the junction exist yet?)",
            docs.display()
        )
    })?;
    // `GetFinalPathNameByHandleW`'s default form is VOLUME_NAME_DOS
    // (`\\?\`-prefixed) — strip it for an ordinary Win32 path, the same
    // convention `Session::launch`'s own `strip_verbatim` already applies to
    // this exact API's output shape.
    let stripped = resolved.strip_prefix(r"\\?\").unwrap_or(&resolved);
    Ok(PathBuf::from(stripped))
}

/// Whether `resolved` (root 1's OS-resolved real location) is the same
/// directory as `expected` (this process's own configured `profiles` dir),
/// canonicalising both first so a trailing separator or an alternate-but-
/// equivalent spelling does not read as a mismatch.
fn resolved_target_matches(resolved: &Path, expected: &Path) -> bool {
    let a = std::fs::canonicalize(resolved).unwrap_or_else(|_| resolved.to_path_buf());
    let b = std::fs::canonicalize(expected).unwrap_or_else(|_| expected.to_path_buf());
    a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

/// Wraps another provider, tallying getattr/open/read/write traffic
/// locally, so `skyrim-live` can print a genuinely root-scoped count even
/// though `vfs_director::io_stats` (what `print_open_totals` already prints)
/// has no root dimension at all. Mounted only at `RootId(1)` — root 0's own
/// provider is left completely untouched, so this adds observability
/// without risking any change to root 0's existing, already-measured
/// behaviour.
struct CountingProvider {
    inner: Arc<dyn Provider>,
    getattr_ok: AtomicU64,
    getattr_notfound: AtomicU64,
    getattr_err: AtomicU64,
    open_read_ok: AtomicU64,
    open_read_err: AtomicU64,
    open_write_ok: AtomicU64,
    open_write_err: AtomicU64,
    reads: AtomicU64,
    read_bytes: AtomicU64,
    writes: AtomicU64,
    write_bytes: AtomicU64,
}

impl CountingProvider {
    fn new(inner: Arc<dyn Provider>) -> Self {
        CountingProvider {
            inner,
            getattr_ok: AtomicU64::new(0),
            getattr_notfound: AtomicU64::new(0),
            getattr_err: AtomicU64::new(0),
            open_read_ok: AtomicU64::new(0),
            open_read_err: AtomicU64::new(0),
            open_write_ok: AtomicU64::new(0),
            open_write_err: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
        }
    }

    /// `(ok, err)` across *both* read and write opens — the same shape
    /// `vfs_director::io_stats::open_totals()` reports globally, so
    /// `print_open_totals` can subtract this from the global pair to get
    /// root 0's implied share.
    fn open_totals(&self) -> (u64, u64) {
        let ord = Ordering::Relaxed;
        (
            self.open_read_ok.load(ord) + self.open_write_ok.load(ord),
            self.open_read_err.load(ord) + self.open_write_err.load(ord),
        )
    }

    /// Human-readable, self-contained root-1 section for `print_open_totals`.
    fn report(&self) -> String {
        let ord = Ordering::Relaxed;
        format!(
            "  root 1: getattr ok={} notfound={} err={} | open read ok={} err={} write ok={} err={} \
| read_ops={} read_bytes={} | write_ops={} write_bytes={}\n",
            self.getattr_ok.load(ord),
            self.getattr_notfound.load(ord),
            self.getattr_err.load(ord),
            self.open_read_ok.load(ord),
            self.open_read_err.load(ord),
            self.open_write_ok.load(ord),
            self.open_write_err.load(ord),
            self.reads.load(ord),
            self.read_bytes.load(ord),
            self.writes.load(ord),
            self.write_bytes.load(ord),
        )
    }
}

impl Provider for CountingProvider {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        let r = self.inner.getattr(p);
        let ord = Ordering::Relaxed;
        match &r {
            Ok(Some(_)) => {
                self.getattr_ok.fetch_add(1, ord);
            }
            Ok(None) => {
                self.getattr_notfound.fetch_add(1, ord);
            }
            Err(_) => {
                self.getattr_err.fetch_add(1, ord);
            }
        }
        r
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.inner.readdir(p)
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let is_write = flags & OPEN_WRITE != 0;
        let r = self.inner.open(p, flags);
        let ord = Ordering::Relaxed;
        match (r.is_ok(), is_write) {
            (true, false) => {
                self.open_read_ok.fetch_add(1, ord);
            }
            (false, false) => {
                self.open_read_err.fetch_add(1, ord);
            }
            (true, true) => {
                self.open_write_ok.fetch_add(1, ord);
            }
            (false, true) => {
                self.open_write_err.fetch_add(1, ord);
            }
        }
        r
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        self.inner.close(h)
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let r = self.inner.read_at(h, offset, buf);
        if let Ok(n) = r {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.read_bytes.fetch_add(n as u64, Ordering::Relaxed);
        }
        r
    }

    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        let r = self.inner.write_at(h, offset, buf);
        if let Ok(n) = r {
            self.writes.fetch_add(1, Ordering::Relaxed);
            self.write_bytes.fetch_add(n as u64, Ordering::Relaxed);
        }
        r
    }

    fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
        self.inner.set_len(h, len)
    }

    fn flush(&self, h: Handle) -> Result<(), i32> {
        self.inner.flush(h)
    }

    fn mkdir(&self, p: VPath) -> Result<(), i32> {
        self.inner.mkdir(p)
    }

    fn remove(&self, p: VPath) -> Result<(), i32> {
        self.inner.remove(p)
    }

    fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
        self.inner.rename(from, to)
    }

    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        self.inner.set_attr(p, attr)
    }
}

/// If the zip has a single top-level directory, return its name.
fn detect_zip_root_prefix(be: &dyn Provider) -> Result<Option<String>, String> {
    let entries = be
        .readdir(VPath::at_default(""))
        .map_err(|e| format!("readdir root: {e}"))?;
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
///
/// The overlay copy is written directly with `std::fs`, not through the
/// shim's `Overlay` type (this runs before a `Session`/`Engine` exists at
/// all) — but it still has to land exactly where `Engine`'s local overlay
/// will look for it once the shim is live: `Engine::decide`'s DRM-exception
/// path for `steam_appid.txt` (`hook.rs`) checks the overlay *before* ever
/// reaching the director, so this write and that lookup must agree on the
/// physical path or the file is invisible to the VFS (gate 4, Task 2's
/// review round 1: this used to be `overrides.join("steam_appid.txt")`,
/// which stopped matching once the overlay became root-scoped). Root 0 is
/// the only root a plain, single-`Session` binary like this one has, so
/// `RootId::DEFAULT` is not a guess to revisit later — it is what root 0
/// *is* here.
fn write_steam_appid(root: &Path, overrides: &Path) -> Result<(), String> {
    let body = format!("{SKYRIM_SE_APP_ID}\n");
    // Physical next to host (anything that bypasses VFS early).
    let on_disk = root.join("steam_appid.txt");
    std::fs::write(&on_disk, &body).map_err(|e| format!("write {}: {e}", on_disk.display()))?;
    // Overlay so dual-layer/VFS open of <root>\steam_appid.txt sees it too —
    // at the same root-scoped subdirectory `Engine`'s local overlay (and the
    // director's own mounted layer over it, see `overlay_layer_dir`'s doc
    // comment) both resolve against.
    let overlay_dir = vfs_director::overlay_layer_dir(overrides, RootId::DEFAULT);
    std::fs::create_dir_all(&overlay_dir)
        .map_err(|e| format!("mkdir {}: {e}", overlay_dir.display()))?;
    let in_overlay = overlay_dir.join("steam_appid.txt");
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
        let mut last_state;
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

/// Director-side open counts, for reconciling against the shim's `routed`
/// outcome count (see `rust/docs/bypass-baseline.md`).
///
/// `skyrim-live` embeds the director directly rather than through the
/// `vfs-directord` gRPC daemon, so there is no `vfs stats` endpoint to query
/// for a live game run; this prints the same `io_stats::open_totals()` /
/// `rejected_writes()` the gRPC `stats` RPC exposes for the daemon case.
/// Purely additive stderr output — no I/O routing decision reads this.
///
/// **Per-root breakdown, to the extent this process can see one.**
/// `vfs_director::io_stats` has no root dimension at all —
/// `ring_dispatch.rs` calls `record_open`/`record_getattr`/etc. with a bare
/// vpath, never a `RootId`, so `open_totals()` and `rejected_writes()` below
/// are sums across *every* mounted root, root 1 included. `root1` is this
/// process's own local tally (see `CountingProvider`), wrapping exactly the
/// provider mounted at `RootId(1)` — root 0's own provider is left
/// untouched, so root 0's contribution is reported here only as "global
/// minus root 1", not measured directly. That is the honest limit of what
/// this task can report without changing `vfs-director` itself.
fn print_open_totals(root1: &CountingProvider) {
    let (ok, err) = vfs_director::io_stats::open_totals();
    let rejected = vfs_director::io_stats::rejected_writes();
    let rejected_total: u64 = rejected.iter().map(|(_, c)| *c).sum();
    eprintln!(
        "  vfs-io opens: ok={ok} err={err} (reconciliation target ok+err={}) rejected_writes={} distinct path(s), {rejected_total} total (both roots combined — see per-root breakdown below)",
        ok + err,
        rejected.len()
    );
    let (root1_open_ok, root1_open_err) = root1.open_totals();
    eprintln!(
        "  root 0 (implied = combined − root 1): open ok={} err={}",
        ok.saturating_sub(root1_open_ok),
        err.saturating_sub(root1_open_err)
    );
    eprint!("{}", root1.report());
    eprintln!(
        "  root 1's provider is unconditionally ReadWrite (an OverlayProvider whose upper is a \
DiskProvider), so a director-level write rejection (the {rejected_total} count above) is \
expected to be entirely root 0's — root 1 would only ever contribute an `open write err` above, \
never a rejected_writes entry. Root 0 is now copy-on-write too, so its own rejected_writes \
should be near zero: a write refused by the read-only layers is copied up instead of refused"
    );
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

/// Returns the `Documents\My Games\Skyrim Special Edition` path (the
/// junction this function creates, pointing at `profiles`) so `run` can
/// declare it as the second managed root without re-deriving `%USERPROFILE%`
/// a second time and risking drift between the two computations.
fn setup_my_games_junctions(profiles: &Path, saves: &Path) -> Result<PathBuf, String> {
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
    Ok(docs)
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

/// Gate-3 task 1, GAP 2: `skyrim-live` (the harness Task 5's real-launch
/// verification depends on) used to write DX-redist DLLs straight onto disk
/// under `root` and stage the launch EXE in a directory the provider graph
/// never covered. Both are real files sitting where a fully virtual root
/// would need a provider to answer for them. These tests exercise
/// `mount_low_priority_disk_layers` — the fix — directly: `getattr`/`open`
/// must succeed through the director, not merely "the file happens to exist
/// on disk".
#[cfg(test)]
mod staging_layer_tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "vfs-skyrim-live-test-{}-{name}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A DX-redist-style file physically dropped straight into `root` (as
    /// `stage_dx_redist` does) must resolve through the director's provider
    /// graph, not only be "a file that happens to be on disk".
    #[test]
    fn a_file_dropped_straight_into_root_resolves_through_the_provider_graph() {
        let root = tmp("root");
        let staged_dir = tmp("staged"); // deliberately not yet created/populated
        std::fs::write(root.join("X3DAudio1_7.dll"), b"REDIST-BYTES").unwrap();

        let session = Session::new();
        mount_low_priority_disk_layers(&session, &root, &staged_dir).unwrap();

        let st = session
            .kernel()
            .getattr(RootId::DEFAULT, "X3DAudio1_7.dll")
            .unwrap()
            .expect("a file physically in root must resolve through the provider graph");
        assert_eq!(st.kind, vfs_protocol::KIND_FILE);
        assert_eq!(st.size, "REDIST-BYTES".len() as u64);
        let (fh, size, is_dir) = session
            .kernel()
            .open(RootId::DEFAULT, "X3DAudio1_7.dll", vfs_protocol::OPEN_READ)
            .expect("open must succeed through the provider graph");
        assert!(!is_dir);
        let mut buf = vec![0u8; size as usize];
        let n = session.kernel().read(fh, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"REDIST-BYTES");
        let _ = session.kernel().close(fh);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&staged_dir);
    }

    /// Real game content must still win over a same-named file physically
    /// sitting in `root` — `mount_low_priority_disk_layers` is meant to be
    /// mounted *before* content precisely so this holds.
    #[test]
    fn real_content_wins_over_a_same_named_file_in_root() {
        let root = tmp("root-precedence");
        let staged_dir = tmp("staged-precedence");
        std::fs::write(root.join("shared.dll"), b"FROM-ROOT").unwrap();

        let session = Session::new();
        mount_low_priority_disk_layers(&session, &root, &staged_dir).unwrap();

        let content_dir = tmp("content-precedence");
        std::fs::write(content_dir.join("shared.dll"), b"FROM-CONTENT").unwrap();
        session
            .mount("", Arc::new(DiskProvider::new(&content_dir)))
            .unwrap();

        let bytes = session.read_file("shared.dll").unwrap();
        assert_eq!(
            bytes, b"FROM-CONTENT",
            "content mounted after the low-priority disk layers must win"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&staged_dir);
        let _ = std::fs::remove_dir_all(&content_dir);
    }

    /// The staging directory is mounted *before* `stage_launch_with` ever
    /// runs (see the doc comment on `mount_low_priority_disk_layers`).
    /// `DiskProvider` reads lazily, so a file written there afterward — the
    /// same order `run()` uses — must still resolve through the provider
    /// graph once it exists.
    #[test]
    fn the_staging_directory_resolves_once_populated_even_though_mounted_first() {
        let root = tmp("root-late");
        let staged_dir = tmp("staged-late");

        let session = Session::new();
        mount_low_priority_disk_layers(&session, &root, &staged_dir).unwrap();

        assert!(
            session.kernel().getattr(RootId::DEFAULT, "SkyrimSE.exe").unwrap().is_none(),
            "must not be visible before staging happens"
        );

        // What `stage_launch_with` would do later in `run()`.
        std::fs::write(staged_dir.join("SkyrimSE.exe"), b"STAGED-EXE-BYTES").unwrap();

        let st = session
            .kernel()
            .getattr(RootId::DEFAULT, "SkyrimSE.exe")
            .unwrap()
            .expect("staged file must resolve through the provider graph once written");
        assert_eq!(st.size, "STAGED-EXE-BYTES".len() as u64);
        let bytes = session.read_file("SkyrimSE.exe").unwrap();
        assert_eq!(bytes, b"STAGED-EXE-BYTES");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&staged_dir);
    }
}
