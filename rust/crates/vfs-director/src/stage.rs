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
//! Staging the *real* image also keeps the launch on the proven path: the host
//! is then byte-identical to the image being hollowed, so the loader's TLS,
//! `.pdata` and LDR metadata already describe it (see `host_is_target` in
//! `vfs-inject`). A substitute host has to hand-replicate all of that.
//!
//! # Lifetime
//!
//! [`StagedDir`] deletes the directory on drop, but the EXE is mapped while the
//! process runs, so the caller must hold it until the child exits. A crashed
//! launcher leaks one directory; [`sweep_stale`] reclaims those on the next run.

use std::path::{Path, PathBuf};

/// Directory-name prefix for staged launches. Deliberately distinct from the
/// `vfs-run-` / `vfs-sse-` / `vfs-sec-` prefixes that `vfs-inject` treats as
/// forbidden PE staging — those mark *content* extraction, which is still
/// disallowed; this is the pre-boot loader closure.
pub const STAGE_PREFIX: &str = "vfs-stage-";

/// Upper bound on staged files, so a malformed import table cannot turn a
/// launch into an unbounded extraction.
const MAX_STAGED_FILES: usize = 64;

/// A staged launch directory. Deleted on drop.
#[derive(Debug)]
pub struct StagedDir {
    dir: PathBuf,
    /// Absolute path of the staged EXE (the `CreateProcess` image).
    exe: PathBuf,
    staged: Vec<String>,
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
        remove_staged_dir(&self.dir)
    }
}

impl Drop for StagedDir {
    fn drop(&mut self) {
        let _ = remove_staged_dir(&self.dir);
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
pub fn stage_launch(
    source: &dyn ImageSource,
    exe_vpath: &str,
    root: &Path,
    tag: &str,
    fallback_dirs: &[PathBuf],
) -> Result<StagedDir, String> {
    let exe_name = Path::new(exe_vpath)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("no file name in {exe_vpath}"))?
        .to_string();

    let exe_bytes = source
        .read(exe_vpath)
        .ok_or_else(|| format!("VFS has no {exe_vpath}"))?;
    if !vfs_inject::pe_looks_like_image(&exe_bytes) {
        return Err(format!("{exe_vpath} is not a PE image"));
    }

    let dir = root.join(format!("{STAGE_PREFIX}{tag}"));
    // A leftover from a previous run with the same tag would shadow us.
    let _ = remove_staged_dir(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;

    let exe_path = dir.join(&exe_name);
    std::fs::write(&exe_path, &exe_bytes)
        .map_err(|e| format!("write {}: {e}", exe_path.display()))?;

    let mut staged = vec![exe_name.clone()];
    let mut pending: Vec<Vec<u8>> = vec![exe_bytes];
    let mut seen: Vec<String> = vec![exe_name.to_ascii_lowercase()];

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
            let dest = dir.join(&base);
            std::fs::write(&dest, &bytes)
                .map_err(|e| format!("write {}: {e}", dest.display()))?;
            staged.push(base);
            pending.push(bytes);
        }
    }

    Ok(StagedDir {
        dir,
        exe: exe_path,
        staged,
    })
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
