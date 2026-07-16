//! Launch Skyrim (via SKSE) with game/mod content served straight from
//! Stored ZIP archives under `C:\GameLayers`.
//!
//! **Zero archive extract:** no PE/BSA/ESP bytes are written to the managed
//! root or TEMP. Primary EXE is process-hollowed from zip bytes into a
//! pre-existing host image (WriteProcessMemory only). Data/ and PE DLLs are
//! `Decision::Serve` zip windows (SEC_IMAGE via in-process manual map).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use vfs_core::{decode, EntryKind, Layer, LayerId, Source, SourceId};
use vfs_zip::{read_layer, ZipBackend};

const DEFAULT_LAYERS: &str = r"C:\GameLayers";
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
           --probe          Run vfs-game-probe against the VFS (no game)\n\
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
        // "1. Skyrim Special Edition.zip" → 1
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

/// Read bytes from a zip-window source blob (or plain disk path).
fn read_source_bytes(source: &SourceId, size: u64) -> Result<Vec<u8>, String> {
    match decode(&source.0) {
        Source::ZipWindow { offset, container } => {
            let path = String::from_utf8_lossy(container);
            let mut f = std::fs::File::open(path.as_ref())
                .map_err(|e| format!("open container {path}: {e}"))?;
            use std::io::{Read, Seek, SeekFrom};
            f.seek(SeekFrom::Start(offset))
                .map_err(|e| format!("seek {path} @{offset}: {e}"))?;
            let mut buf = vec![0u8; size as usize];
            f.read_exact(&mut buf)
                .map_err(|e| format!("read {path} @{offset} len={size}: {e}"))?;
            Ok(buf)
        }
        Source::Disk(bytes) => {
            let path = String::from_utf8_lossy(bytes);
            std::fs::read(path.as_ref()).map_err(|e| format!("read {path}: {e}"))
        }
    }
}

/// Empty directory skeleton only — **never** writes archive file content.
/// PE entries keep zip-window sources (hollow / SEC_IMAGE manual map at runtime).
fn prepare_layer(layer: Layer, root: &Path) -> Result<Layer, String> {
    for entry in &layer.entries {
        let dest = root.join(entry.vpath.replace('/', "\\"));
        match entry.kind {
            EntryKind::Dir => {
                std::fs::create_dir_all(&dest)
                    .map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
            }
            EntryKind::File => {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
                }
                // Intentionally no PE materialize — zip-window sources retained.
            }
            EntryKind::Tombstone => {}
        }
    }
    Ok(layer)
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

fn find_pe_bytes(layers: &[Layer], file_name: &str) -> Result<Vec<u8>, String> {
    let want = file_name.to_ascii_lowercase();
    let mut found: Option<&vfs_core::InputEntry> = None;
    for layer in layers {
        for e in &layer.entries {
            if e.kind != EntryKind::File {
                continue;
            }
            let base = e
                .vpath
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(&e.vpath)
                .to_ascii_lowercase();
            if base == want {
                found = Some(e);
            }
        }
    }
    let e = found.ok_or_else(|| format!("PE {file_name} not in layer zips"))?;
    read_source_bytes(&e.source, e.size)
}

fn locate_artifacts() -> Result<(String, String), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dll = vfs_inject::find_near(&exe, "vfs_shim_dll.dll")
        .or_else(|| {
            // Workspace target/debug layout when running `cargo run -p vfs-launch`.
            let mut d = exe.clone();
            for _ in 0..5 {
                if let Some(p) = d.parent() {
                    d = p.to_path_buf();
                    let c = d.join("vfs_shim_dll.dll");
                    if c.is_file() {
                        return Some(c);
                    }
                    let c = d.join("debug").join("vfs_shim_dll.dll");
                    if c.is_file() {
                        return Some(c);
                    }
                }
            }
            None
        })
        .ok_or_else(|| "vfs_shim_dll.dll not found near vfs-launch".to_string())?;
    let payload = vfs_inject::ensure_payload_beside_shim(dll.to_str().unwrap(), None)
        .ok_or_else(|| "vfs_payload.dll not found".to_string())?;
    Ok((dll.to_string_lossy().into_owned(), payload))
}

fn set_steam_env() {
    // Prefer env over steam_appid.txt so we don't add another file under the root.
    std::env::set_var("SteamAppId", STEAM_APPID);
    std::env::set_var("SteamGameId", STEAM_APPID);
}

/// Canonical early load-order for SSE masters (only those present are emitted).
const MASTER_ORDER: &[&str] = &[
    "Skyrim.esm",
    "Update.esm",
    "Dawnguard.esm",
    "HearthFires.esm",
    "Dragonborn.esm",
];

/// Collect plugin basenames (`*.esm` / `*.esp` / `*.esl`) from layer entries under `Data/`.
fn collect_plugins(layers: &[Layer]) -> Vec<String> {
    let mut seen = std::collections::BTreeMap::<String, String>::new(); // fold -> display
    for layer in layers {
        for e in &layer.entries {
            if e.kind != EntryKind::File {
                continue;
            }
            let v = e.vpath.replace('\\', "/");
            let Some(name) = v.strip_prefix("Data/").or_else(|| v.strip_prefix("data/")) else {
                continue;
            };
            if name.contains('/') {
                continue; // only top-level Data plugins
            }
            let lower = name.to_ascii_lowercase();
            if !(lower.ends_with(".esm") || lower.ends_with(".esp") || lower.ends_with(".esl")) {
                continue;
            }
            seen.insert(lower, name.to_string());
        }
    }
    // Order: fixed masters first, then remaining esm/esl alphabetically, then esp alphabetically.
    let mut out = Vec::new();
    let mut rest = seen;
    for m in MASTER_ORDER {
        let key = m.to_ascii_lowercase();
        if let Some(name) = rest.remove(&key) {
            out.push(name);
        }
    }
    let mut esm_esl: Vec<String> = rest
        .iter()
        .filter(|(k, _)| k.ends_with(".esm") || k.ends_with(".esl"))
        .map(|(_, v)| v.clone())
        .collect();
    esm_esl.sort_by_key(|s| s.to_ascii_lowercase());
    for name in esm_esl {
        rest.remove(&name.to_ascii_lowercase());
        out.push(name);
    }
    let mut esp: Vec<String> = rest.into_values().collect();
    esp.sort_by_key(|s| s.to_ascii_lowercase());
    out.extend(esp);
    out
}

/// SSE `Plugins.txt` / `loadorder.txt` bodies for the given plugin basenames.
/// `Plugins.txt` uses a leading `*` to mark enabled plugins.
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

/// Write Plugins.txt + loadorder.txt under `%LOCALAPPDATA%\Skyrim Special Edition`
/// so layer mods (e.g. SkyUI) are enabled. Config only — not archive extraction.
fn ensure_plugins_enabled(layers: &[Layer]) -> Result<(), String> {
    let plugins = collect_plugins(layers);
    if plugins.is_empty() {
        return Ok(());
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "LOCALAPPDATA not set".to_string())?
        .join("Skyrim Special Edition");
    std::fs::create_dir_all(&base).map_err(|e| format!("mkdir {}: {e}", base.display()))?;
    let plugins_body = format_plugins_txt(&plugins);
    let loadorder_body = format_loadorder_txt(&plugins);
    // UTF-8 is what this install accepts; game rewrites Plugins.txt after read.
    std::fs::write(base.join("Plugins.txt"), plugins_body.as_bytes())
        .map_err(|e| format!("write Plugins.txt: {e}"))?;
    std::fs::write(base.join("loadorder.txt"), loadorder_body.as_bytes())
        .map_err(|e| format!("write loadorder.txt: {e}"))?;
    eprintln!(
        "  enabled {} plugins in {} (incl. SkyUI if present)",
        plugins.len(),
        base.display()
    );
    for p in &plugins {
        eprintln!("    *{p}");
    }
    Ok(())
}

#[cfg(test)]
mod plugin_order_tests {
    use super::*;
    use vfs_core::{EntryKind, InputEntry, Layer, LayerId, SourceId};

    fn file(vpath: &str) -> InputEntry {
        InputEntry {
            vpath: vpath.into(),
            kind: EntryKind::File,
            source: SourceId::new(b"x".as_slice()),
            size: 1,
            mtime: 0,
        }
    }

    #[test]
    fn collect_orders_masters_then_cc_then_skyui() {
        let layers = vec![
            Layer {
                id: LayerId(0),
                entries: vec![
                    file("Data/Skyrim.esm"),
                    file("Data/Update.esm"),
                    file("Data/Dawnguard.esm"),
                    file("Data/ccBGSSSE001-Fish.esm"),
                    file("Data/_ResourcePack.esl"),
                ],
            },
            Layer {
                id: LayerId(2),
                entries: vec![file("Data/SkyUI_SE.esp")],
            },
        ];
        let p = collect_plugins(&layers);
        assert_eq!(p.first().map(String::as_str), Some("Skyrim.esm"));
        assert!(p.iter().any(|x| x == "SkyUI_SE.esp"));
        assert_eq!(p.last().map(String::as_str), Some("SkyUI_SE.esp"));
        let body = format_plugins_txt(&p);
        assert!(body.contains("*SkyUI_SE.esp"));
        assert!(body.contains("*Skyrim.esm"));
    }
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

    eprintln!("building VFS snapshot (all zip-window; zero archive→disk writes)…");
    let mut layers: Vec<Layer> = Vec::new();
    for (i, zip) in zips.iter().enumerate() {
        eprintln!("  parsing {} …", zip.file_name().unwrap_or_default().to_string_lossy());
        let layer = match read_layer(zip, LayerId(i as u32)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("error: read_layer {}: {e:?}", zip.display());
                std::process::exit(1);
            }
        };
        eprintln!("    {} entries (zip-window retained)", layer.entries.len());
        let prepared = match prepare_layer(layer, &args.root) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("error: prepare_layer: {e}");
                std::process::exit(1);
            }
        };
        layers.push(prepared);
    }

    let pe_name = if args.use_skse {
        "skse64_loader.exe"
    } else {
        "SkyrimSE.exe"
    };
    let pe_bytes = if args.probe {
        None
    } else {
        match find_pe_bytes(&layers, pe_name) {
            Ok(b) => {
                eprintln!(
                    "  loaded {pe_name} from zip into RAM ({} bytes) — hollow, no disk write",
                    b.len()
                );
                Some(b)
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    };

    if let Err(e) = ensure_plugins_enabled(&layers) {
        eprintln!("warning: plugins enablement failed: {e}");
    }

    // Host session: configure → mount zip backends → serve → launch (I/O remapped).
    let mut session = vfs_director::Session::new();
    session.set_root(&args.root);
    session.set_overlay(&args.overlay);
    session.set_state_dir(&args.state);
    for zip in &zips {
        match ZipBackend::open(zip) {
            Ok(be) => {
                if let Err(e) = session.mount("", Arc::new(be)) {
                    eprintln!("error: mount {}: status {e}", zip.display());
                    std::process::exit(1);
                }
                eprintln!("  mounted backend {}", zip.display());
            }
            Err(e) => {
                eprintln!("error: ZipBackend {}: {e:?}", zip.display());
                std::process::exit(1);
            }
        }
    }
    if let Err(e) = session.serve() {
        eprintln!("error: serve: {e}");
        std::process::exit(1);
    }
    eprintln!("  director serving IPC for remapped I/O");

    set_steam_env();

    // Probe: host-side read through the director kernel (no inject).
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
        let report_path = args.state.join("probe-report.txt");
        let _ = std::fs::write(&report_path, &report);
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
    eprintln!("launching {pe_name} under {} …", args.root.display());
    eprintln!("  mode: {} + hollow from VFS + remapped I/O", if detach { "detach" } else { "wait" });

    let _ = pe_bytes; // validated above; launch reloads PE from VFS for hollow
    let exit = session.launch(&vfs_director::LaunchOpts {
        image: pe_name.to_string(),
        args: vec![],
        wait: args.wait,
        hollow_pe: true,
        shim_dll: None,
        payload_dll: None,
    });

    match exit {
        Ok(code) => {
            if detach {
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
