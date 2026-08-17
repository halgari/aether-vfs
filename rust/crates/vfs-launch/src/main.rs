//! Launch Skyrim (via SKSE) with game/mod content served from Stored ZIP layers.
//!
//! Host workflow: discover zips → `Session` mount → serve IPC → launch (I/O remapped).
//!
//! **The launch image is not staged here, despite what this file used to say.**
//! It hands `Session::launch` the PE's *vpath*, and `Session::launch` resolves
//! a relative image on real disk under the managed root — so this works only
//! when `--root` is a directory that already holds the exe, and not with the
//! default root (`<layers>/runtime`, created empty and payload-wiped). Reading
//! the image out of the provider graph, writing it plus its PE import closure
//! to disk and mounting that back under the real content is
//! `vfs-directord`'s `SessionRegistry::launch`; it has never been wired into
//! this tool. `Session::launch` now says so by name instead of failing in
//! `CreateProcess`.

use std::path::{Path, PathBuf};

use vfs_embed::{Director, DiskProvider, RootId, KIND_FILE, Session};

const DEFAULT_LAYERS: &str = r"C:	mp";
const STEAM_APPID: &str = "489830"; // Skyrim Special Edition

fn usage() -> ! {
    eprintln!(
        "Usage: vfs-launch [options]\n\
         \n\
         Launch Skyrim Special Edition from zip layers without extracting assets.\n\
         \n\
         Options:\n\
           --layers <dir>   Directory with numbered layer zips (default: {DEFAULT_LAYERS})\n\
           --root <dir>     Managed game root (default: <layers>/runtime)\n\
           --overlay <dir>  Write overlay (default: <layers>/overlay)\n\
           --state <dir>    Config/ready flags (default: <layers>/vfs-state)\n\
           --se             Launch SkyrimSE.exe instead of skse64_loader.exe\n\
           --wait           Wait for the game process to exit (default: detach)\n\
           --probe          Probe VFS paths via Session (no game)\n\
           --help           Show this help\n"
    );
    std::process::exit(2);
}

struct Args {
    layers_dir: PathBuf,
    root: PathBuf,
    overlay: PathBuf,
    state: PathBuf,
    use_skse: bool,
    wait: bool,
    probe: bool,
}

fn parse_args() -> Args {
    let mut layers_dir = PathBuf::from(DEFAULT_LAYERS);
    let mut root: Option<PathBuf> = None;
    let mut overlay: Option<PathBuf> = None;
    let mut state: Option<PathBuf> = None;
    let mut use_skse = true;
    let mut wait = false;
    let mut probe = false;

    let mut argv = std::env::args().skip(1);
    while let Some(a) = argv.next() {
        match a.as_str() {
            "--help" | "-h" => usage(),
            "--layers" => {
                layers_dir = PathBuf::from(argv.next().unwrap_or_else(|| usage()));
            }
            "--root" => {
                root = Some(PathBuf::from(argv.next().unwrap_or_else(|| usage())));
            }
            "--overlay" => {
                overlay = Some(PathBuf::from(argv.next().unwrap_or_else(|| usage())));
            }
            "--state" => {
                state = Some(PathBuf::from(argv.next().unwrap_or_else(|| usage())));
            }
            "--se" => use_skse = false,
            "--wait" => wait = true,
            "--probe" => probe = true,
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }

    Args {
        root: root.unwrap_or_else(|| layers_dir.join("runtime")),
        overlay: overlay.unwrap_or_else(|| layers_dir.join("overlay")),
        state: state.unwrap_or_else(|| layers_dir.join("vfs-state")),
        layers_dir,
        use_skse,
        wait,
        probe,
    }
}

/// Discover numbered layer zips: `1. …zip`, `2. …zip`, … sorted by the leading number.
fn discover_layers(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found: Vec<(u32, PathBuf)> = Vec::new();
    let rd = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    for ent in rd.flatten() {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("zip"))
            != Some(true)
        {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let num: u32 = name
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if num > 0 {
            found.push((num, path));
        }
    }
    found.sort_by_key(|(n, _)| *n);
    if found.is_empty() {
        return Err(format!("no numbered layer zips in {}", dir.display()));
    }
    Ok(found.into_iter().map(|(_, p)| p).collect())
}

/// Remove any leftover archive payload files under `root` (prior PE bootstrap).
fn wipe_payload_files(root: &Path) -> Result<usize, String> {
    fn walk(dir: &Path, n: &mut usize) -> Result<(), String> {
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                walk(&p, n)?;
            } else if p.is_file() {
                std::fs::remove_file(&p).map_err(|e| format!("remove {}: {e}", p.display()))?;
                *n += 1;
            }
        }
        Ok(())
    }
    let mut n = 0;
    walk(root, &mut n)?;
    Ok(n)
}

fn set_steam_env() {
    std::env::set_var("SteamAppId", STEAM_APPID);
    std::env::set_var("SteamGameId", STEAM_APPID);
}

const MASTER_ORDER: &[&str] = &[
    "Skyrim.esm",
    "Update.esm",
    "Dawnguard.esm",
    "HearthFires.esm",
    "Dragonborn.esm",
];

/// Order plugin basenames: fixed masters, other esm/esl, then esp.
fn order_plugins(mut seen: std::collections::BTreeMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    for m in MASTER_ORDER {
        let key = m.to_ascii_lowercase();
        if let Some(name) = seen.remove(&key) {
            out.push(name);
        }
    }
    let mut esm_esl: Vec<String> = seen
        .iter()
        .filter(|(k, _)| k.ends_with(".esm") || k.ends_with(".esl"))
        .map(|(_, v)| v.clone())
        .collect();
    esm_esl.sort_by_key(|s| s.to_ascii_lowercase());
    for name in esm_esl {
        seen.remove(&name.to_ascii_lowercase());
        out.push(name);
    }
    let mut esp: Vec<String> = seen.into_values().collect();
    esp.sort_by_key(|s| s.to_ascii_lowercase());
    out.extend(esp);
    out
}

/// Collect top-level `Data/*.{esm,esp,esl}` from the mounted director kernel.
fn collect_plugins_from_kernel(kernel: &Director) -> Vec<String> {
    let mut seen = std::collections::BTreeMap::<String, String>::new();
    let Ok(entries) = kernel.readdir(RootId::DEFAULT, "Data") else {
        return Vec::new();
    };
    for e in entries {
        if e.stat.kind != KIND_FILE {
            continue;
        }
        let lower = e.name.to_ascii_lowercase();
        if !(lower.ends_with(".esm") || lower.ends_with(".esp") || lower.ends_with(".esl")) {
            continue;
        }
        seen.insert(lower, e.name);
    }
    order_plugins(seen)
}

pub fn format_plugins_txt(plugins: &[String]) -> String {
    let mut s = String::from(
        "# This file is used by Skyrim to keep track of your downloaded content.\n\
         # Please do not modify this file.\n",
    );
    for p in plugins {
        s.push('*');
        s.push_str(p);
        s.push('\n');
    }
    s
}

pub fn format_loadorder_txt(plugins: &[String]) -> String {
    let mut s = String::new();
    for p in plugins {
        s.push_str(p);
        s.push('\n');
    }
    s
}

fn ensure_plugins_enabled(plugins: &[String]) -> Result<(), String> {
    if plugins.is_empty() {
        return Ok(());
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "LOCALAPPDATA not set".to_string())?
        .join("Skyrim Special Edition");
    std::fs::create_dir_all(&base).map_err(|e| format!("mkdir {}: {e}", base.display()))?;
    std::fs::write(base.join("Plugins.txt"), format_plugins_txt(plugins).as_bytes())
        .map_err(|e| format!("write Plugins.txt: {e}"))?;
    std::fs::write(base.join("loadorder.txt"), format_loadorder_txt(plugins).as_bytes())
        .map_err(|e| format!("write loadorder.txt: {e}"))?;
    eprintln!(
        "  enabled {} plugins in {} (incl. SkyUI if present)",
        plugins.len(),
        base.display()
    );
    for p in plugins {
        eprintln!("    *{p}");
    }
    Ok(())
}

/// Find PE by basename under the VFS (any directory), case-insensitive.
fn find_pe_vpath(kernel: &Director, file_name: &str) -> Option<String> {
    let want = file_name.to_ascii_lowercase();
    // Common game roots first.
    for dir in ["", "Data"] {
        let Ok(entries) = kernel.readdir(RootId::DEFAULT, dir) else {
            continue;
        };
        for e in entries {
            if e.stat.kind == KIND_FILE && e.name.to_ascii_lowercase() == want {
                return Some(if dir.is_empty() {
                    e.name
                } else {
                    format!("{dir}/{}", e.name)
                });
            }
        }
    }
    // Fallback: open by bare name (casefold backends).
    if kernel.getattr(RootId::DEFAULT, file_name).ok().flatten().is_some() {
        return Some(file_name.to_string());
    }
    None
}

fn count_payload_files(root: &Path) -> usize {
    fn walk(dir: &Path, n: &mut usize) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                walk(&p, n);
            } else if p.is_file() {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(root, &mut n);
    n
}

fn main() {
    let args = parse_args();

    eprintln!("vfs-launch: layers={}", args.layers_dir.display());
    let zips = match discover_layers(&args.layers_dir) {
        Ok(z) => z,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    for (i, z) in zips.iter().enumerate() {
        eprintln!("  layer {i}: {}", z.display());
    }

    std::fs::create_dir_all(&args.root).expect("create game root");
    std::fs::create_dir_all(&args.overlay).expect("create overlay");
    std::fs::create_dir_all(&args.state).expect("create state dir");
    match wipe_payload_files(&args.root) {
        Ok(0) => {}
        Ok(n) => eprintln!("  wiped {n} leftover payload file(s) under {}", args.root.display()),
        Err(e) => {
            eprintln!("error: wipe root: {e}");
            std::process::exit(1);
        }
    }

    // Configure → mount (single CD parse per zip via ZipProvider) → serve → launch.
    let mut session = Session::new();
    session.set_root(&args.root);
    session.set_overlay(&args.overlay);
    session.set_state_dir(&args.state);
    for zip in &zips {
        if let Err(e) = session.mount_zip(zip) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        eprintln!("  mounted backend {}", zip.display());
    }

    // A copy-on-write layer over the zips (gate 4, Task 6). Without one, every
    // mount here is read-only, so the director refuses every write under the
    // root — including an in-place edit of zip content, which is the one
    // write a modded game does most. Before Task 5 that refusal fell through
    // to the shim's own local overlay, which the director cannot read back;
    // now it is a hard failure at the NT boundary. The layer addresses the
    // same root-scoped subdirectory the shim's overlay uses, so the two
    // agree on one physical location for root 0's writes.
    let write_layer = session.overlay_layer_dir(RootId::DEFAULT);
    if let Err(e) = std::fs::create_dir_all(&write_layer) {
        eprintln!("error: create write layer {}: {e}", write_layer.display());
        std::process::exit(1);
    }
    if let Err(st) = session.set_write_layer(std::sync::Arc::new(DiskProvider::new(&write_layer))) {
        eprintln!("error: set write layer status {st}");
        std::process::exit(1);
    }
    eprintln!("  writes copy up into {}", write_layer.display());

    let pe_name = if args.use_skse {
        "skse64_loader.exe"
    } else {
        "SkyrimSE.exe"
    };
    let pe_vpath = find_pe_vpath(session.kernel(), pe_name).unwrap_or_else(|| pe_name.to_string());
    if !args.probe {
        match session.kernel().getattr(RootId::DEFAULT, &pe_vpath) {
            Ok(Some(st)) if st.kind == KIND_FILE && st.size > 512 => {
                eprintln!(
                    "  PE {pe_vpath} present in VFS ({} bytes) — must also be on disk                      under the managed root; see this file's header on staging",
                    st.size
                );
            }
            _ => {
                eprintln!("error: PE {pe_name} not found in mounted layers");
                std::process::exit(1);
            }
        }
    }

    let plugins = collect_plugins_from_kernel(session.kernel());
    if let Err(e) = ensure_plugins_enabled(&plugins) {
        eprintln!("warning: plugins enablement failed: {e}");
    }

    if let Err(e) = session.serve() {
        eprintln!("error: serve: {e}");
        std::process::exit(1);
    }
    eprintln!("  director serving IPC for remapped I/O");

    set_steam_env();

    if args.probe {
        let mut report = String::new();
        report.push_str("authority=director-session\n");
        let checks = [
            "Data/Skyrim.esm",
            "Data/SkyUI_SE.esp",
            "Data/SkyUI_SE.bsa",
        ];
        let mut ok_n = 0u32;
        for vpath in checks {
            match session.read_file(vpath) {
                Ok(bytes) => {
                    let n = bytes.len().min(8);
                    report.push_str(&format!(
                        "read {vpath}: ok len={} head={:02x?}\n",
                        bytes.len(),
                        &bytes[..n]
                    ));
                    if vpath.ends_with(".esm") || vpath.ends_with(".esp") {
                        if bytes.len() >= 4 && &bytes[..4] == b"TES4" {
                            report.push_str(&format!("magic {vpath}: TES4 ok\n"));
                            ok_n += 1;
                        } else {
                            report.push_str(&format!(
                                "magic {vpath}: FAIL head={:02x?}\n",
                                &bytes[..bytes.len().min(4)]
                            ));
                        }
                    } else if !bytes.is_empty() {
                        ok_n += 1;
                    }
                }
                Err(e) => report.push_str(&format!("read {vpath}: ERR status {e}\n")),
            }
        }
        let payload_files = count_payload_files(&args.root);
        report.push_str(&format!("root_payload_files={payload_files}\n"));
        report.push_str(&format!("probe_ok_paths={ok_n}\n"));
        let _ = std::fs::write(args.state.join("probe-report.txt"), &report);
        eprintln!("--- probe report ---\n{report}");
        if ok_n < 2 || payload_files != 0 {
            eprintln!("error: probe failed (need >=2 good paths and zero root payloads)");
            std::process::exit(1);
        }
        eprintln!("probe ok");
        session.stop_serve();
        std::process::exit(0);
    }

    let leftover = wipe_payload_files(&args.root).unwrap_or(0);
    if leftover > 0 {
        eprintln!("  re-wiped {leftover} files before launch");
    }

    let detach = !args.wait;
    eprintln!("launching {pe_vpath} under {} …", args.root.display());
    eprintln!(
        "  mode: {} + remapped I/O (image read from disk, NOT staged from the VFS)",
        if detach { "detach" } else { "wait" }
    );

    let exit = session.launch(&vfs_embed::LaunchOpts {
        image: pe_vpath,
        args: vec![],
        wait: args.wait,
        shim_dll: None,
        payload_dll: None,
        env: Default::default(),
    });

    match exit {
        Ok(code) => {
            if detach {
                // Session (and IPC workers) must outlive the child; forget keeps them alive
                // until this host process exits.
                eprintln!("game process running (detached); keep host process alive for IPC");
                std::mem::forget(session);
            } else {
                eprintln!("game exited with code {code}");
                session.stop_serve();
                std::process::exit(code);
            }
        }
        Err(e) => {
            eprintln!("error: launch failed: {e}");
            session.stop_serve();
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod plugin_order_tests {
    use super::*;

    #[test]
    fn order_masters_then_cc_then_skyui() {
        let mut seen = std::collections::BTreeMap::new();
        for n in [
            "Skyrim.esm",
            "Update.esm",
            "Dawnguard.esm",
            "ccBGSSSE001-Fish.esm",
            "_ResourcePack.esl",
            "SkyUI_SE.esp",
        ] {
            seen.insert(n.to_ascii_lowercase(), n.to_string());
        }
        let p = order_plugins(seen);
        assert_eq!(p.first().map(String::as_str), Some("Skyrim.esm"));
        assert!(p.iter().any(|x| x == "SkyUI_SE.esp"));
        assert_eq!(p.last().map(String::as_str), Some("SkyUI_SE.esp"));
        let body = format_plugins_txt(&p);
        assert!(body.contains("*SkyUI_SE.esp"));
        assert!(body.contains("*Skyrim.esm"));
    }
}
