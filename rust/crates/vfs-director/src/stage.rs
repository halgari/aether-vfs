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
    if !vfs_inject::pe_looks_like_image(&exe_bytes) {
        return Err(format!("{exe_vpath} is not a PE image"));
    }

    let exe_path = target_dir.join(&exe_name);
    std::fs::write(&exe_path, &exe_bytes)
        .map_err(|e| format!("write {}: {e}", exe_path.display()))?;
    staged_dir.files.push(exe_path.clone());

    // Names and written paths accumulate locally and are merged at the end:
    // holding `&mut staged_dir.staged` across the loop would rule out
    // recording each write in `staged_dir.files`, which the non-owning
    // cleanup path needs.
    let mut staged: Vec<String> = std::mem::take(&mut staged_dir.staged);
    let mut written: Vec<PathBuf> = Vec::new();
    staged.push(exe_name.clone());
    let mut pending: Vec<Vec<u8>> = vec![exe_bytes];
    // Already-staged names carry across calls, so a second image does not
    // restage shared dependencies.
    let mut seen: Vec<String> = staged.iter().map(|s| s.to_ascii_lowercase()).collect();

    // Breadth-first over the import graph: a staged DLL can itself import
    // another game-local DLL, and the loader needs the whole closure present.
    while let Some(pe) = pending.pop() {
        let Some(imports) = vfs_inject::import_dll_names_of_pe(&pe) else {
            continue;
        };
        for imp in imports {
            let base = Path::new(&imp)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&imp)
                .to_string();
            let key = base.to_ascii_lowercase();
            if seen.contains(&key) || vfs_inject::is_system_import_dll(&base) {
                continue;
            }
            seen.push(key);
            // Siblings of the EXE inside the VFS.
            let vpath = match Path::new(exe_vpath).parent().and_then(|p| p.to_str()) {
                Some(p) if !p.is_empty() => format!("{}/{base}", p.replace('\\', "/")),
                _ => base.clone(),
            };
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
            if !vfs_inject::pe_looks_like_image(&bytes) {
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
            std::fs::write(&dest, &bytes)
                .map_err(|e| format!("write {}: {e}", dest.display()))?;
            written.push(dest);
            staged.push(base);
            pending.push(bytes);
        }
    }

    staged_dir.staged = staged;
    staged_dir.files.extend(written);
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

    struct Fake(HashMap<String, Vec<u8>>);
    impl ImageSource for Fake {
        fn read(&self, vpath: &str) -> Option<Vec<u8>> {
            self.0.get(vpath).cloned()
        }
    }

    fn tmp_root(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vfs-stage-test-{}-{name}", std::process::id()));
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
}
