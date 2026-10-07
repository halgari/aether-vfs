//! Stage a launch directory: the target EXE plus the static imports the Windows
//! loader must resolve before our shim exists.
//!
//! # Why anything reaches disk at all
//!
//! Game content is served from the VFS and never extracted. But two things
//! happen before the shim can serve anything:
//!
//! 1. `CreateProcess` needs a real on-disk image. Windows cannot create a
//!    process from bytes.
//! 2. The loader resolves the EXE's static imports during process init, which
//!    runs *before* our hooks are installed. A game EXE alone in a directory
//!    dies `0xC0000135` (`STATUS_DLL_NOT_FOUND`) at that point.
//!
//! So we stage exactly the PE closure — the EXE and its non-system imports,
//! transitively — and nothing else. For Skyrim SE that is the 37 MiB EXE plus a
//! couple of DLLs, against ~15 GiB of archive that stays virtual.
//!
//! Staging the *real* image is also what lets the Windows loader do its own
//! job: it maps, relocates and binds the PE, and builds the TLS template,
//! `.pdata` registration and LDR metadata that describe it. An earlier design
//! hand-replicated all of that over a substitute host image; staging removed
//! the need (see `architecture.md` §4.2).
//!
//! # Proxy DLLs beside the EXE
//!
//! The import walk skips system DLLs: they resolve from `System32`, and
//! parsing them is not ours to do. But Windows searches the **application
//! directory before `System32`** for every DLL that is not a KnownDLL, and a
//! whole class of mods depends on exactly that: ReShade ships as `dxgi.dll` /
//! `d3d11.dll` / `d3d9.dll` / `opengl32.dll`, ENB as `d3d11.dll` (plus its
//! `d3dcompiler_46e.dll`), plugin loaders as `dinput8.dll` / `version.dll` /
//! `winmm.dll` / `winhttp.dll`, and so on. A proxy like that is often reached
//! not from the EXE's import table at all but through a system DLL we never
//! parse — under Proton, DXVK's `d3d11.dll` imports `dxgi.dll`, and the loader
//! resolves that at process init, before the shim exists. If the proxy is not
//! on real disk in the EXE's directory then, the loader silently falls back to
//! `System32` and the mod never loads (no `ReShade.log`, no open of
//! `game\dxgi.dll` in the shim trace).
//!
//! So [`stage_into`] also probes the VFS for every name in
//! [`PROXY_DLL_NAMES`] beside each staged image, independent of any import
//! table, stages what it finds, and walks *those* images' imports too. The
//! set deliberately excludes [`KNOWN_DLLS`]: Windows never loads those from
//! the app directory, and a game shipping a stray copy of one (`msvcp140.dll`,
//! `kernel32.dll`) must keep today's behaviour rather than have Wine load it
//! from the game folder.
//!
//! # Where it lands, and why that is not a detail
//!
//! Images keep their vpath position, and [`stage_launch_into`] puts them
//! inside the **virtual root** rather than in a sibling directory.
//!
//! A process resolves far more than its imports from its own module path, and
//! none of that is the loader's doing or ours to enumerate. Cyberpunk 2077
//! derives its game root as `exeDir/../..`; Stardew Valley's .NET apphost
//! looks for its managed assembly, `runtimeconfig.json` and `deps.json` beside
//! the EXE — and that assembly is not a static import at all, so its PE
//! closure is empty and staging copies exactly one file. Put the EXE outside
//! the virtual root and every one of those reads lands on real disk where the
//! shim never sees it: Cyberpunk exits 0 in silence, Stardew fails
//! `LibHostAppRootFindFailure`. Skyrim SE was unaffected only by coincidence —
//! its EXE sits at the game root and it finds content through the cwd, which
//! *is* set to the virtual root.
//!
//! # Lifetime
//!
//! The EXE is mapped while the process runs, so the caller must hold the
//! [`StagedDir`] until the child exits. [`stage_launch`] owns the directory it
//! made and removes it on drop; [`stage_launch_into`] removes only the files
//! it wrote and prunes only the directories it created, because the directory
//! belongs to the caller. A crashed launcher leaks whatever it staged;
//! [`sweep_stale`] reclaims the owned-directory form on the next run.

use std::path::{Path, PathBuf};

/// Directory-name prefix for staged launches. Deliberately distinct from the
/// `vfs-run-` / `vfs-sse-` / `vfs-sec-` prefixes that `vfs-inject` treats as
/// forbidden PE staging — those mark *content* extraction, which is still
/// disallowed; this is the pre-boot loader closure.
pub const STAGE_PREFIX: &str = "vfs-stage-";

/// Upper bound on staged files, so a malformed import table cannot turn a
/// launch into an unbounded extraction.
const MAX_STAGED_FILES: usize = 64;

/// DLL names a game may carry beside its EXE as a **proxy** — a replacement
/// for a system DLL that the loader picks up from the application directory
/// because the app directory is searched before `System32` for anything that
/// is not a KnownDLL.
///
/// Each is probed in the VFS beside every staged image and staged when
/// present, whether or not anything's import table names it (see the module
/// docs: the import is often made by a system DLL staging does not parse).
///
/// Two groups, one rule — "what a real install's app directory would supply
/// to the loader at init":
///
/// * Every name [`vfs_pe::is_system_import_dll`] classifies as system that is
///   **not** in [`KNOWN_DLLS`]. These are the ones the import walk skips, so
///   without the probe they could never be staged.
/// * Proxy names that are not system-classified but are commonly reached
///   through a system DLL rather than the EXE (`d3d9`, `opengl32`, `dsound`,
///   the other Direct3D and DirectInput/XInput versions, ENB's
///   `d3dcompiler_46e`). A direct import of one of these was already staged
///   by the walk; the probe covers the indirect case.
///
/// Lower-case: VFS lookups are case-insensitive, and the staged file takes
/// this spelling (Wine's file lookups are case-insensitive too).
pub const PROXY_DLL_NAMES: &[&str] = &[
    // System-classified (vfs_pe::is_system_import_dll) and not KnownDLLs.
    "d3d11.dll",
    "dxgi.dll",
    "dinput8.dll",
    "version.dll",
    "winmm.dll",
    "winhttp.dll",
    "dbghelp.dll",
    "xinput1_3.dll",
    "xinput1_4.dll",
    "x3daudio1_7.dll",
    "hid.dll",
    "dwmapi.dll",
    "uxtheme.dll",
    "wintrust.dll",
    "userenv.dll",
    // Common proxy names the import walk would stage only on a direct import.
    "d3d8.dll",
    "d3d9.dll",
    "d3d10.dll",
    "d3d10_1.dll",
    "d3d10core.dll",
    "d3d12.dll",
    "ddraw.dll",
    "opengl32.dll",
    "dsound.dll",
    "dinput.dll",
    "xinput1_1.dll",
    "xinput1_2.dll",
    "xinput9_1_0.dll",
    "wininet.dll",
    "d3dcompiler_46e.dll",
];

/// True KnownDLLs (and the CRT/API-set DLLs treated the same way): Windows
/// maps these from `System32` regardless of what sits in the app directory, so
/// they are **never** proxies and never probed. A game folder carrying a copy
/// of one keeps today's behaviour: the import walk skips it as a system DLL
/// and the probe never asks for it. Loading one of these from the game folder
/// under Wine could break the process outright.
///
/// Kept beside [`PROXY_DLL_NAMES`] so a test can prove the two are disjoint.
pub const KNOWN_DLLS: &[&str] = &[
    "kernel32.dll",
    "kernelbase.dll",
    "ntdll.dll",
    "user32.dll",
    "gdi32.dll",
    "gdi32full.dll",
    "advapi32.dll",
    "shell32.dll",
    "ole32.dll",
    "oleaut32.dll",
    "sechost.dll",
    "rpcrt4.dll",
    "combase.dll",
    "shlwapi.dll",
    "imm32.dll",
    "ws2_32.dll",
    "setupapi.dll",
    "bcrypt.dll",
    "bcryptprimitives.dll",
    "crypt32.dll",
    "psapi.dll",
    "ucrtbase.dll",
    "msvcp140.dll",
    "vcruntime140.dll",
    "vcruntime140_1.dll",
];

/// Whether `name` must never be staged as a proxy: a KnownDLL, a CRT DLL in
/// the same position, or an API set (`api-ms-*` / `ext-ms-*`).
fn is_known_dll(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("api-ms-")
        || n.starts_with("ext-ms-")
        || n.starts_with("vcruntime140")
        || KNOWN_DLLS.contains(&n.as_str())
}

/// A staged launch directory.
///
/// Two ownership modes, because there are two places staging can land:
///
/// * `owns_dir` — the directory was created for this launch
///   ([`stage_launch`]), and dropping removes the whole thing.
/// * not `owns_dir` — the images were written *into* a directory the caller
///   owns, normally the virtual root ([`stage_launch_into`]). Dropping removes
///   exactly the files written and prunes the directories created for them.
///   Removing the directory itself would delete the managed root.
#[derive(Debug)]
pub struct StagedDir {
    dir: PathBuf,
    /// Absolute path of the staged EXE (the `CreateProcess` image).
    exe: PathBuf,
    staged: Vec<String>,
    /// The subset of `staged` that is in [`PROXY_DLL_NAMES`], in order.
    proxies: Vec<String>,
    /// Absolute paths written, for the non-owning cleanup path.
    files: Vec<PathBuf>,
    /// Absolute paths of directories created, deepest last so pruning can walk
    /// them in reverse.
    created_dirs: Vec<PathBuf>,
    owns_dir: bool,
}

impl StagedDir {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// Names staged, in the order they were resolved (EXE first).
    pub fn staged(&self) -> &[String] {
        &self.staged
    }

    /// Proxy DLLs ([`PROXY_DLL_NAMES`]) the VFS carries beside a staged image,
    /// lower case, whether this staging wrote them or a same-named file was
    /// already on disk in the caller's directory.
    ///
    /// A Wine/Proton launcher needs these: Wine loads a native `dinput8.dll`
    /// or `version.dll` from the app directory only when `WINEDLLOVERRIDES`
    /// asks for native first, so these are the candidates for `name=n,b`.
    pub fn proxies(&self) -> &[String] {
        &self.proxies
    }

    /// Delete now instead of at drop, reporting failure.
    ///
    /// Windows keeps the image file locked until the process fully exits, so
    /// call this only after waiting on the child.
    pub fn cleanup(&self) -> Result<(), String> {
        if self.owns_dir {
            return remove_staged_dir(&self.dir);
        }
        let mut failed: Vec<String> = Vec::new();
        for f in &self.files {
            if let Err(e) = std::fs::remove_file(f) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    failed.push(format!("{}: {e}", f.display()));
                }
            }
        }
        // Deepest first, and only if empty: a directory that already existed,
        // or that the game has since written into, is not ours to remove.
        for d in self.created_dirs.iter().rev() {
            let _ = std::fs::remove_dir(d);
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(format!("remove staged files: {}", failed.join("; ")))
        }
    }
}

impl Drop for StagedDir {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// Refuse to delete anything that is not one of our staging directories.
fn remove_staged_dir(dir: &Path) -> Result<(), String> {
    let is_ours = dir
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n.starts_with(STAGE_PREFIX));
    if !is_ours {
        return Err(format!("refusing to remove non-staging dir {}", dir.display()));
    }
    if !dir.exists() {
        return Ok(());
    }
    std::fs::remove_dir_all(dir).map_err(|e| format!("remove {}: {e}", dir.display()))
}

/// Delete staging directories left behind by earlier runs.
///
/// A launcher killed mid-run cannot delete its own directory, so reclaim any
/// under `root` that no longer have a live owner. Returns how many were removed.
pub fn sweep_stale(root: &Path) -> usize {
    let mut n = 0;
    let Ok(rd) = std::fs::read_dir(root) else {
        return 0;
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if !name.starts_with(STAGE_PREFIX) {
            continue;
        }
        // A directory whose EXE is still mapped by a running process cannot be
        // removed; treat that failure as "still in use" and leave it.
        if remove_staged_dir(&ent.path()).is_ok() {
            n += 1;
        }
    }
    n
}

/// Reads a virtual path out of the VFS. Implemented by the caller so this
/// module stays independent of how content is served.
pub trait ImageSource {
    /// Whole-file bytes for `vpath`, or `None` when absent.
    fn read(&self, vpath: &str) -> Option<Vec<u8>>;
}

/// Stage `exe_vpath` and its transitive non-system static imports under `root`.
///
/// `tag` distinguishes concurrent launches (a pid, or a counter for children).
///
/// `fallback_dirs` are searched on disk for imports the VFS does not carry —
/// redistributables such as `d3dx9_42.dll` are static imports of the game but
/// ship with the DirectX runtime, not in the game archive, so without this the
/// loader would fail them at process init.
///
/// Imports found in neither are skipped rather than failing: system DLLs
/// resolve from `System32`, and a missing optional import is the loader's
/// problem to report, not ours to guess at.
/// Additional images to stage alongside the primary, each with its own import
/// closure.
///
/// A launcher that spawns the real game (SKSE's `skse64_loader.exe` starts
/// `SkyrimSE.exe`) needs its target beside it on disk: `CreateProcess` in the
/// child needs a real image just as much as the first one did. Staging both
/// into one directory satisfies that without the launcher's spawn having to be
/// intercepted and staged in turn.
pub fn stage_launch_with(
    source: &dyn ImageSource,
    exe_vpath: &str,
    also: &[&str],
    root: &Path,
    tag: &str,
    fallback_dirs: &[PathBuf],
) -> Result<StagedDir, String> {
    let mut staged_dir = stage_launch(source, exe_vpath, root, tag, fallback_dirs)?;
    for extra in also {
        stage_into(source, extra, &mut staged_dir, fallback_dirs)?;
    }
    Ok(staged_dir)
}

/// The vpath's directory part, rejecting anything that could escape the base.
///
/// A vpath comes from a provider graph, not a user, but it becomes a path
/// under a directory we then write to — `..` or a root component must never
/// reach `join`.
fn safe_parent(exe_vpath: &str) -> Result<PathBuf, String> {
    let normalized = exe_vpath.replace('\\', "/");
    let mut out = PathBuf::new();
    let mut parts: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    parts.pop(); // the file name
    for p in parts {
        if p == ".." || p == "." || p.contains(':') {
            return Err(format!("refusing to stage {exe_vpath}: unsafe path component {p:?}"));
        }
        out.push(p);
    }
    Ok(out)
}

pub fn stage_launch(
    source: &dyn ImageSource,
    exe_vpath: &str,
    root: &Path,
    tag: &str,
    fallback_dirs: &[PathBuf],
) -> Result<StagedDir, String> {
    let dir = root.join(format!("{STAGE_PREFIX}{tag}"));
    // A leftover from a previous run with the same tag would shadow us.
    let _ = remove_staged_dir(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;

    let mut staged_dir = new_staged_dir(&dir, exe_vpath, true)?;
    stage_into(source, exe_vpath, &mut staged_dir, fallback_dirs)?;
    Ok(staged_dir)
}

/// Stage into `dir` itself, with no per-launch subdirectory.
///
/// `dir` is the caller's — normally the **virtual root** — and is never
/// removed; only the files written are.
///
/// This exists because where the image lands decides what the game can find.
/// A staged EXE outside the virtual root takes everything the process resolves
/// relative to its own module path with it, and those reads land on real disk
/// where the shim never sees them. Cyberpunk 2077 derives its game root as
/// `exeDir/../..` and quietly exits; Stardew Valley's .NET apphost looks for
/// its managed assembly beside the EXE and fails
/// `LibHostAppRootFindFailure`. Neither is a missing-import problem, so no
/// amount of import-closure staging fixes them — the EXE has to sit at its own
/// vpath inside the root.
pub fn stage_launch_into(
    source: &dyn ImageSource,
    exe_vpath: &str,
    also: &[&str],
    dir: &Path,
    fallback_dirs: &[PathBuf],
) -> Result<StagedDir, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let mut staged_dir = new_staged_dir(dir, exe_vpath, false)?;
    stage_into(source, exe_vpath, &mut staged_dir, fallback_dirs)?;
    for extra in also {
        stage_into(source, extra, &mut staged_dir, fallback_dirs)?;
    }
    Ok(staged_dir)
}

/// Write one staged image, and record it for cleanup **only if staging is
/// what put it there**.
///
/// When staging into a directory the caller owns, a destination that already
/// exists belongs to the caller and is left exactly as found. The managed root
/// is seeded with DirectX redistributables before any launch, and several of
/// them — `X3DAudio1_7.dll`, `d3dx9_42.dll` — are also static imports of the
/// game. Treating those as staged output overwrote them with identical bytes
/// and then deleted them on cleanup, so the *second* launch of a loadout
/// failed with "VFS has no X3DAudio1_7.dll". Staging must never remove a file
/// it did not create.
///
/// "Already exists" is case-insensitive: the host filesystem is not, but the
/// loader's is, and a caller's `DXGI.dll` next to a staged `dxgi.dll` would be
/// two files that Wine sees as one name.
fn write_staged(dest: &Path, bytes: &[u8], staged_dir: &mut StagedDir) -> Result<(), String> {
    if !staged_dir.owns_dir && exists_ignoring_case(dest) {
        return Ok(());
    }
    std::fs::write(dest, bytes).map_err(|e| format!("write {}: {e}", dest.display()))?;
    staged_dir.files.push(dest.to_path_buf());
    Ok(())
}

fn exists_ignoring_case(path: &Path) -> bool {
    if path.exists() {
        return true;
    }
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return false;
    };
    let Ok(rd) = std::fs::read_dir(parent) else {
        return false;
    };
    rd.flatten().any(|e| {
        e.file_name()
            .to_str()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

/// The vpath of `name` beside `exe_vpath` — how the import walk and the proxy
/// probe both address an image's siblings.
fn sibling_vpath(exe_vpath: &str, name: &str) -> String {
    match Path::new(exe_vpath).parent().and_then(|p| p.to_str()) {
        Some(p) if !p.is_empty() => format!("{}/{name}", p.replace('\\', "/")),
        _ => name.to_string(),
    }
}

/// Create `base/rel` level by level, recording only the levels that did not
/// already exist so cleanup prunes exactly what staging added.
fn create_dir_tracked(base: &Path, rel: &Path, staged_dir: &mut StagedDir) -> Result<(), String> {
    let mut cur = base.to_path_buf();
    for comp in rel.components() {
        cur.push(comp);
        if cur.exists() {
            continue;
        }
        std::fs::create_dir(&cur).map_err(|e| format!("mkdir {}: {e}", cur.display()))?;
        if !staged_dir.created_dirs.contains(&cur) {
            staged_dir.created_dirs.push(cur.clone());
        }
    }
    Ok(())
}

fn new_staged_dir(dir: &Path, exe_vpath: &str, owns_dir: bool) -> Result<StagedDir, String> {
    let exe_name = Path::new(exe_vpath)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("no file name in {exe_vpath}"))?
        .to_string();
    Ok(StagedDir {
        dir: dir.to_path_buf(),
        exe: dir.join(safe_parent(exe_vpath)?).join(&exe_name),
        staged: Vec::new(),
        proxies: Vec::new(),
        files: Vec::new(),
        created_dirs: Vec::new(),
        owns_dir,
    })
}

/// Add `exe_vpath` and its import closure to an existing staged directory.
fn stage_into(
    source: &dyn ImageSource,
    exe_vpath: &str,
    staged_dir: &mut StagedDir,
    fallback_dirs: &[PathBuf],
) -> Result<(), String> {
    let dir = staged_dir.dir.clone();
    let exe_name = Path::new(exe_vpath)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("no file name in {exe_vpath}"))?
        .to_string();
    // Imports are siblings of the EXE, so this is where both it and they go.
    // Preserving it is what keeps `exeDir` meaningful to the game and what
    // makes the staging mount answer at the vpath it was read from.
    let rel_dir = safe_parent(exe_vpath)?;
    let target_dir = dir.join(&rel_dir);
    create_dir_tracked(&dir, &rel_dir, staged_dir)?;

    let exe_bytes = source
        .read(exe_vpath)
        .ok_or_else(|| format!("VFS has no {exe_vpath}"))?;
    if !vfs_pe::pe_looks_like_image(&exe_bytes) {
        return Err(format!("{exe_vpath} is not a PE image"));
    }

    let exe_path = target_dir.join(&exe_name);
    write_staged(&exe_path, &exe_bytes, staged_dir)?;

    // Names and written paths accumulate locally and are merged at the end:
    // holding `&mut staged_dir.staged` across the loop would rule out
    // recording each write in `staged_dir.files`, which the non-owning
    // cleanup path needs.
    let mut staged: Vec<String> = std::mem::take(&mut staged_dir.staged);
    // Collected rather than written inline: `write_staged` needs &mut
    // staged_dir, which `staged` is borrowed out of for the loop.
    let mut writes: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    staged.push(exe_name.clone());
    let mut pending: Vec<Vec<u8>> = vec![exe_bytes];
    // Already-staged names carry across calls, so a second image does not
    // restage shared dependencies.
    let mut seen: Vec<String> = staged.iter().map(|s| s.to_ascii_lowercase()).collect();
    let mut proxies: Vec<String> = std::mem::take(&mut staged_dir.proxies);

    // Proxy DLLs beside the image, whatever imports them (module docs). Only
    // the image's own directory: that is the loader's application directory,
    // so a proxy anywhere else would not be picked up by a real install
    // either. Each one found joins `pending`, so its own non-system imports
    // are staged by the walk below.
    for &name in PROXY_DLL_NAMES {
        // The constants are disjoint (tested); this keeps it so at runtime
        // if one list is edited without the other.
        if is_known_dll(name) || seen.iter().any(|s| s == name) {
            continue;
        }
        let Some(bytes) = source.read(&sibling_vpath(exe_vpath, name)) else {
            continue;
        };
        if !vfs_pe::pe_looks_like_image(&bytes) {
            continue;
        }
        if staged.len() >= MAX_STAGED_FILES {
            return Err(format!(
                "import closure exceeded {MAX_STAGED_FILES} files at {name}"
            ));
        }
        seen.push(name.to_string());
        writes.push((target_dir.join(name), bytes.clone()));
        staged.push(name.to_string());
        proxies.push(name.to_string());
        pending.push(bytes);
    }

    // Breadth-first over the import graph: a staged DLL can itself import
    // another game-local DLL, and the loader needs the whole closure present.
    while let Some(pe) = pending.pop() {
        let Some(imports) = vfs_pe::import_dll_names_of_pe(&pe) else {
            continue;
        };
        for imp in imports {
            // `Path::file_name()` splits on `\` only on Windows; these import
            // names come from a Windows PE and must split on either separator
            // regardless of host OS. Same precedent as
            // `vfs_pe::is_system_import_dll`.
            let base = imp.rsplit(['/', '\\']).next().unwrap_or(&imp).to_string();
            let key = base.to_ascii_lowercase();
            if seen.contains(&key) || vfs_pe::is_system_import_dll(&base) {
                continue;
            }
            seen.push(key);
            // Siblings of the EXE inside the VFS.
            let vpath = sibling_vpath(exe_vpath, &base);
            let from_disk = || {
                fallback_dirs.iter().find_map(|d| {
                    // Case-insensitive: archives and redist packages disagree
                    // on casing (`D3DX9_42.dll` vs `d3dx9_42.dll`).
                    let direct = d.join(&base);
                    if direct.is_file() {
                        return std::fs::read(&direct).ok();
                    }
                    std::fs::read_dir(d).ok()?.flatten().find_map(|e| {
                        e.file_name()
                            .to_str()
                            .is_some_and(|n| n.eq_ignore_ascii_case(&base))
                            .then(|| std::fs::read(e.path()).ok())
                            .flatten()
                    })
                })
            };
            let Some(bytes) = source
                .read(&vpath)
                .or_else(|| source.read(&base))
                .or_else(from_disk)
            else {
                continue;
            };
            if !vfs_pe::pe_looks_like_image(&bytes) {
                continue;
            }
            if staged.len() >= MAX_STAGED_FILES {
                return Err(format!(
                    "import closure exceeded {MAX_STAGED_FILES} files at {base}"
                ));
            }
            // Beside the EXE, not at the staging root: an import of
            // `bin/x64/Cyberpunk2077.exe` is `bin/x64/PhysX3_x64.dll`, and the
            // loader looks for it in the EXE's own directory.
            let dest = target_dir.join(&base);
            writes.push((dest, bytes.clone()));
            staged.push(base);
            pending.push(bytes);
        }
    }

    staged_dir.staged = staged;
    staged_dir.proxies = proxies;
    for (dest, bytes) in writes {
        write_staged(&dest, &bytes, staged_dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Minimal PE: MZ header, e_lfanew, PE32+ optional header, no imports.
    fn bare_pe() -> Vec<u8> {
        let mut pe = vec![0u8; 0x400];
        pe[0] = b'M';
        pe[1] = b'Z';
        pe[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        // COFF: machine x64, 0 sections
        pe[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
        // SizeOfOptionalHeader
        pe[0x94..0x96].copy_from_slice(&240u16.to_le_bytes());
        // Optional header magic PE32+
        pe[0x98..0x9A].copy_from_slice(&0x20Bu16.to_le_bytes());
        pe
    }

    /// Minimal PE32+ whose import table names `imports`, so the walk has
    /// something to follow. No sections: headers span the whole image, and
    /// the descriptors and names live in them.
    fn pe_importing(imports: &[&str]) -> Vec<u8> {
        let mut pe = bare_pe();
        pe.resize(0x1000, 0);
        // SizeOfImage and SizeOfHeaders (optional header +56 / +60).
        pe[0xD0..0xD4].copy_from_slice(&0x1000u32.to_le_bytes());
        pe[0xD4..0xD8].copy_from_slice(&0x1000u32.to_le_bytes());
        let desc_base = 0x200usize;
        let mut name_at = 0x800usize;
        for (i, name) in imports.iter().enumerate() {
            let d = desc_base + i * 20;
            pe[d + 12..d + 16].copy_from_slice(&(name_at as u32).to_le_bytes());
            pe[name_at..name_at + name.len()].copy_from_slice(name.as_bytes());
            name_at += name.len() + 1;
        }
        // DataDirectory[IMPORT]: PE32+ data directories start at opt + 112.
        let size = (imports.len() as u32 + 1) * 20;
        pe[0x110..0x114].copy_from_slice(&(desc_base as u32).to_le_bytes());
        pe[0x114..0x118].copy_from_slice(&size.to_le_bytes());
        pe
    }

    /// Lookups are case-insensitive, as the VFS's are: providers resolve a
    /// vpath without regard to case, and staging relies on that.
    struct Fake(HashMap<String, Vec<u8>>);
    impl ImageSource for Fake {
        fn read(&self, vpath: &str) -> Option<Vec<u8>> {
            self.0
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(vpath))
                .map(|(_, v)| v.clone())
        }
    }

    fn tmp_root(name: &str) -> PathBuf {
        let d = vfs_testkit::scratch_path(&format!("vfs-stage-test-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn stages_the_exe_and_deletes_on_drop() {
        let root = tmp_root("basic");
        let mut m = HashMap::new();
        m.insert("SkyrimSE.exe".to_string(), bare_pe());
        let src = Fake(m);

        let dir_path;
        {
            let staged = stage_launch(&src, "SkyrimSE.exe", &root, "1", &[]).expect("stage");
            dir_path = staged.dir().to_path_buf();
            assert!(staged.exe().is_file());
            assert_eq!(staged.exe().file_name().unwrap(), "SkyrimSE.exe");
            assert!(dir_path.file_name().unwrap().to_string_lossy().starts_with(STAGE_PREFIX));
        }
        assert!(!dir_path.exists(), "staging dir must be removed on drop");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Where the image lands is the whole point: a game that derives its root
    /// from its own module path (Cyberpunk 2077 uses `exeDir/../..`) only
    /// works if the EXE keeps its vpath position under the base directory.
    #[test]
    fn preserves_the_vpath_directory_structure() {
        let root = tmp_root("nested");
        let mut m = HashMap::new();
        m.insert("bin/x64/Cyberpunk2077.exe".to_string(), bare_pe());
        m.insert("tools/redmod/bin/redMod.exe".to_string(), bare_pe());
        let src = Fake(m);

        let staged = stage_launch_into(
            &src,
            "bin/x64/Cyberpunk2077.exe",
            &["tools/redmod/bin/redMod.exe"],
            &root,
            &[],
        )
        .expect("stage");

        assert_eq!(staged.exe(), root.join("bin").join("x64").join("Cyberpunk2077.exe"));
        assert!(staged.exe().is_file());
        assert!(root.join("tools/redmod/bin/redMod.exe").is_file());
        // Flattening would have put it here, where `exeDir/../..` is wrong.
        assert!(!root.join("Cyberpunk2077.exe").exists());

        drop(staged);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The non-owning mode stages into a directory the caller owns — the
    /// virtual root — so dropping must remove what it wrote and nothing else.
    #[test]
    fn staging_into_a_caller_owned_dir_leaves_the_dir_and_its_contents() {
        let root = tmp_root("into");
        // Pre-existing content: the managed root already holds DirectX DLLs
        // and steam_appid.txt before any launch.
        std::fs::write(root.join("steam_appid.txt"), b"489830
").unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin").join("keepme.txt"), b"x").unwrap();

        let mut m = HashMap::new();
        m.insert("bin/x64/game.exe".to_string(), bare_pe());
        let src = Fake(m);

        {
            let staged = stage_launch_into(&src, "bin/x64/game.exe", &[], &root, &[]).expect("stage");
            assert!(staged.exe().is_file());
            assert_eq!(staged.dir(), root.as_path());
        }

        assert!(root.is_dir(), "must not delete the caller's directory");
        assert!(root.join("steam_appid.txt").is_file(), "must not touch pre-existing files");
        assert!(root.join("bin").join("keepme.txt").is_file());
        assert!(!root.join("bin").join("x64").join("game.exe").exists(), "staged file must go");
        // `bin/x64` was created by staging, so it is pruned; `bin` existed
        // already and still holds a file, so it stays.
        assert!(!root.join("bin").join("x64").exists(), "created dir must be pruned");
        assert!(root.join("bin").is_dir(), "pre-existing dir must survive");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The regression that broke the *second* launch of every loadout: the
    /// managed root is seeded with DirectX redistributables, several of which
    /// are also static imports of the game. Staging overwrote them with
    /// identical bytes, recorded them as its own, and deleted them on cleanup.
    #[test]
    fn never_removes_a_file_the_caller_already_had() {
        let root = tmp_root("preexisting");
        // Exactly the shape ocm seeds: a DX redistributable in the root that
        // is also reachable through the graph.
        std::fs::write(root.join("X3DAudio1_7.dll"), bare_pe()).unwrap();

        let mut m = HashMap::new();
        m.insert("SkyrimSE.exe".to_string(), bare_pe());
        m.insert("X3DAudio1_7.dll".to_string(), bare_pe());
        let src = Fake(m);

        for launch in 1..=2 {
            let staged =
                stage_launch_into(&src, "SkyrimSE.exe", &["X3DAudio1_7.dll"], &root, &[])
                    .unwrap_or_else(|e| panic!("launch {launch}: {e}"));
            assert!(staged.exe().is_file());
            drop(staged);
            assert!(
                root.join("X3DAudio1_7.dll").is_file(),
                "launch {launch} deleted a file staging did not create"
            );
            assert!(!root.join("SkyrimSE.exe").exists(), "staged exe must be cleaned up");
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_a_vpath_that_would_escape_the_base_directory() {
        let root = tmp_root("escape");
        let mut m = HashMap::new();
        m.insert("../evil.exe".to_string(), bare_pe());
        let src = Fake(m);
        let err = stage_launch_into(&src, "../evil.exe", &[], &root, &[]).unwrap_err();
        assert!(err.contains("unsafe path component"), "got: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A launcher and the game it spawns must land in one directory, so the
    /// child's CreateProcess finds a real image beside the launcher.
    #[test]
    fn stages_a_launcher_alongside_its_target() {
        let root = tmp_root("launcher");
        let mut m = HashMap::new();
        m.insert("skse64_loader.exe".to_string(), bare_pe());
        m.insert("SkyrimSE.exe".to_string(), bare_pe());
        let src = Fake(m);

        let staged = stage_launch_with(
            &src,
            "skse64_loader.exe",
            &["SkyrimSE.exe"],
            &root,
            "1",
            &[],
        )
        .expect("stage");

        assert_eq!(staged.exe().file_name().unwrap(), "skse64_loader.exe");
        assert!(staged.dir().join("SkyrimSE.exe").is_file(), "target must be staged too");
        assert!(staged.staged().iter().any(|s| s == "SkyrimSE.exe"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_exe_is_an_error_not_an_empty_dir() {
        let root = tmp_root("missing");
        let src = Fake(HashMap::new());
        let e = stage_launch(&src, "Nope.exe", &root, "1", &[]).unwrap_err();
        assert!(e.contains("no Nope.exe"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn non_pe_is_rejected() {
        let root = tmp_root("notpe");
        let mut m = HashMap::new();
        m.insert("SkyrimSE.exe".to_string(), b"not a pe at all".to_vec());
        let e = stage_launch(&Fake(m), "SkyrimSE.exe", &root, "1", &[]).unwrap_err();
        assert!(e.contains("not a PE"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_to_delete_a_directory_it_did_not_stage() {
        let root = tmp_root("guard");
        let victim = root.join("not-ours");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("keep.txt"), b"important").unwrap();

        let e = remove_staged_dir(&victim).unwrap_err();
        assert!(e.contains("refusing"), "{e}");
        assert!(victim.join("keep.txt").is_file(), "guard must not delete");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_reclaims_leftovers_but_leaves_other_dirs() {
        let root = tmp_root("sweep");
        let stale = root.join(format!("{STAGE_PREFIX}9999"));
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("x.dll"), b"MZ").unwrap();
        let other = root.join("unrelated");
        std::fs::create_dir_all(&other).unwrap();

        assert_eq!(sweep_stale(&root), 1);
        assert!(!stale.exists());
        assert!(other.exists(), "sweep must only touch staging dirs");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_test_pe_builder_round_trips_through_the_import_parser() {
        let names = vfs_pe::import_dll_names_of_pe(&pe_importing(&["a.dll", "kernel32.dll"]));
        assert_eq!(
            names.unwrap(),
            vec!["a.dll".to_string(), "kernel32.dll".to_string()]
        );
    }

    /// A proxy name must never also be a KnownDLL: Windows would ignore an
    /// app-directory copy of a KnownDLL, and loading one from the game folder
    /// under Wine can break the process.
    #[test]
    fn proxy_names_and_known_dlls_are_disjoint() {
        for name in PROXY_DLL_NAMES {
            assert!(!is_known_dll(name), "{name} is a KnownDLL");
            assert_eq!(
                *name,
                name.to_ascii_lowercase(),
                "{name} must be lower case"
            );
        }
        for name in KNOWN_DLLS {
            assert!(!PROXY_DLL_NAMES.contains(name), "{name} in both lists");
        }
        assert!(is_known_dll("API-MS-WIN-CRT-RUNTIME-L1-1-0.dll"));
        assert!(is_known_dll("vcruntime140_1.dll"));
    }

    /// Journals of Jyggalag's "DLSS 5" is ReShade as `dxgi.dll` in the game
    /// root. `SkyrimSE.exe` does not import dxgi; DXVK's `d3d11.dll` does, and
    /// staging never parses that. The proxy must be on disk anyway.
    #[test]
    fn stages_a_proxy_dll_beside_the_exe_that_nothing_staged_imports() {
        let root = tmp_root("proxy-dxgi");
        let mut m = HashMap::new();
        m.insert(
            "SkyrimSE.exe".to_string(),
            pe_importing(&["d3d11.dll", "kernel32.dll"]),
        );
        m.insert("dxgi.dll".to_string(), bare_pe());
        let src = Fake(m);

        let staged = stage_launch_into(&src, "SkyrimSE.exe", &[], &root, &[]).expect("stage");
        assert!(
            root.join("dxgi.dll").is_file(),
            "proxy dxgi.dll must be staged"
        );
        assert!(staged.staged().iter().any(|s| s == "dxgi.dll"));
        assert_eq!(staged.proxies(), ["dxgi.dll".to_string()]);
        // d3d11.dll is imported but the VFS does not carry it: System32's.
        assert!(!root.join("d3d11.dll").exists());

        drop(staged);
        assert!(
            !root.join("dxgi.dll").exists(),
            "staged proxy must be cleaned up"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same for a nested EXE: the probe looks in the EXE's own directory,
    /// and the proxy lands there.
    #[test]
    fn probes_the_exe_directory_not_the_root() {
        let root = tmp_root("proxy-nested");
        let mut m = HashMap::new();
        m.insert("bin/x64/game.exe".to_string(), bare_pe());
        m.insert("bin/x64/dinput8.dll".to_string(), bare_pe());
        // At the root, not beside the EXE: not the loader's app directory.
        m.insert("version.dll".to_string(), bare_pe());
        let src = Fake(m);

        let staged = stage_launch_into(&src, "bin/x64/game.exe", &[], &root, &[]).expect("stage");
        assert!(root.join("bin/x64/dinput8.dll").is_file());
        assert!(!root.join("version.dll").exists());
        assert!(!root.join("bin/x64/version.dll").exists());
        drop(staged);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A game folder carrying a KnownDLL keeps today's behaviour: not staged.
    #[test]
    fn does_not_stage_a_known_dll_beside_the_exe() {
        let root = tmp_root("proxy-known");
        let mut m = HashMap::new();
        m.insert("SkyrimSE.exe".to_string(), pe_importing(&["kernel32.dll"]));
        m.insert("kernel32.dll".to_string(), bare_pe());
        m.insert("msvcp140.dll".to_string(), bare_pe());
        let src = Fake(m);

        let staged = stage_launch_into(&src, "SkyrimSE.exe", &[], &root, &[]).expect("stage");
        assert!(!root.join("kernel32.dll").exists());
        assert!(!root.join("msvcp140.dll").exists());
        assert!(staged.proxies().is_empty());
        drop(staged);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A proxy already on disk in the caller's root — whatever its casing —
    /// is the caller's: not overwritten, not a second file, not deleted.
    #[test]
    fn leaves_a_proxy_the_caller_already_had_on_disk() {
        for on_disk in ["dxgi.dll", "DXGI.dll"] {
            let root = tmp_root("proxy-preexisting");
            std::fs::write(root.join(on_disk), b"MZ caller's own").unwrap();

            let mut m = HashMap::new();
            m.insert("SkyrimSE.exe".to_string(), bare_pe());
            m.insert("dxgi.dll".to_string(), bare_pe());
            let src = Fake(m);

            for launch in 1..=2 {
                let staged = stage_launch_into(&src, "SkyrimSE.exe", &[], &root, &[])
                    .unwrap_or_else(|e| panic!("{on_disk} launch {launch}: {e}"));
                assert_eq!(staged.proxies(), ["dxgi.dll".to_string()]);
                let dlls: Vec<_> = std::fs::read_dir(&root)
                    .unwrap()
                    .flatten()
                    .filter(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .eq_ignore_ascii_case("dxgi.dll")
                    })
                    .collect();
                assert_eq!(dlls.len(), 1, "{on_disk}: a second casing was written");
                drop(staged);
                assert_eq!(
                    std::fs::read(root.join(on_disk)).unwrap(),
                    b"MZ caller's own",
                    "{on_disk} launch {launch}: caller's file overwritten or deleted"
                );
            }
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// ENB's `d3d11.dll` imports `d3dcompiler_46e.dll`; the proxy's own
    /// non-system imports are part of the closure the loader needs.
    #[test]
    fn stages_a_proxys_own_non_system_imports() {
        let root = tmp_root("proxy-closure");
        let mut m = HashMap::new();
        m.insert("SkyrimSE.exe".to_string(), pe_importing(&["d3d11.dll"]));
        m.insert(
            "d3d11.dll".to_string(),
            pe_importing(&["enbhelper.dll", "kernel32.dll", "dxgi.dll"]),
        );
        m.insert("enbhelper.dll".to_string(), bare_pe());
        let src = Fake(m);

        let staged = stage_launch_into(&src, "SkyrimSE.exe", &[], &root, &[]).expect("stage");
        assert!(root.join("d3d11.dll").is_file(), "proxy staged");
        assert!(
            root.join("enbhelper.dll").is_file(),
            "proxy's import staged"
        );
        // dxgi is imported by the proxy but the VFS has none: System32's.
        assert!(!root.join("dxgi.dll").exists());
        assert_eq!(staged.proxies(), ["d3d11.dll".to_string()]);
        drop(staged);
        assert!(!root.join("enbhelper.dll").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// VFS lookups ignore case; the proxy is found as `DXGI.DLL` and staged.
    #[test]
    fn finds_a_proxy_whatever_its_case_in_the_vfs() {
        let root = tmp_root("proxy-case");
        let mut m = HashMap::new();
        m.insert("SkyrimSE.exe".to_string(), bare_pe());
        m.insert("DXGI.DLL".to_string(), bare_pe());
        let src = Fake(m);

        let staged = stage_launch_into(&src, "SkyrimSE.exe", &[], &root, &[]).expect("stage");
        assert!(root.join("dxgi.dll").is_file());
        assert_eq!(staged.proxies(), ["dxgi.dll".to_string()]);
        drop(staged);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Haskill launches `skse64_loader.exe` with `SkyrimSE.exe` as an extra
    /// image. Proxies are probed beside each, once.
    #[test]
    fn proxies_are_staged_once_for_a_launcher_and_its_target() {
        let root = tmp_root("proxy-launcher");
        let mut m = HashMap::new();
        m.insert("skse64_loader.exe".to_string(), bare_pe());
        m.insert("SkyrimSE.exe".to_string(), bare_pe());
        m.insert("dxgi.dll".to_string(), bare_pe());
        m.insert("tools/other.exe".to_string(), bare_pe());
        m.insert("tools/version.dll".to_string(), bare_pe());
        let src = Fake(m);

        let staged = stage_launch_into(
            &src,
            "skse64_loader.exe",
            &["SkyrimSE.exe", "tools/other.exe"],
            &root,
            &[],
        )
        .expect("stage");
        assert!(root.join("dxgi.dll").is_file());
        assert!(root.join("tools/version.dll").is_file());
        assert_eq!(
            staged.proxies(),
            ["dxgi.dll".to_string(), "version.dll".to_string()]
        );
        assert_eq!(
            staged.staged().iter().filter(|s| *s == "dxgi.dll").count(),
            1
        );
        drop(staged);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The cap still bounds what probing can add: once the closure is full, a
    /// proxy found beside the next image is an error, not a silent extra file.
    #[test]
    fn proxy_probing_respects_the_staged_file_cap() {
        let root = tmp_root("proxy-cap");
        let mut m = HashMap::new();
        // The first image plus its imports fill the cap exactly.
        let deps: Vec<String> = (1..MAX_STAGED_FILES)
            .map(|i| format!("dep{i}.dll"))
            .collect();
        let refs: Vec<&str> = deps.iter().map(String::as_str).collect();
        m.insert("a.exe".to_string(), pe_importing(&refs));
        for d in &deps {
            m.insert(d.clone(), bare_pe());
        }
        m.insert("sub/b.exe".to_string(), bare_pe());
        m.insert("sub/dxgi.dll".to_string(), bare_pe());
        let src = Fake(m);

        let err = stage_launch_into(&src, "a.exe", &["sub/b.exe"], &root, &[]).unwrap_err();
        assert!(
            err.contains("exceeded") && err.contains("dxgi.dll"),
            "got: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
