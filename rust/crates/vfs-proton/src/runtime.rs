use std::cmp::Ordering;
use std::ffi::OsStr;
use std::io;
use std::path::Path;

use crate::layout::Root;

/// Why a directory did not pass the GE-Proton gate.
///
/// `PROTONPATH` defaults to UMU-Proton (stock Valve Proton) whenever it is
/// unset or points somewhere wrong, so every runtime this crate hands back
/// must pass through here. A non-GE runtime is always an error, never a
/// warning.
#[derive(Debug)]
pub enum VerifyError {
    /// The runtime directory has no `version` file at all.
    Missing,
    /// The `version` file exists but could not be read.
    Unreadable(io::Error),
    /// The `version` file names a build that is not GE-Proton. Carries the
    /// file's trimmed contents so the caller can show what it actually got.
    NotGe(String),
    /// Wine's FFmpeg demuxer (`winedmo.so`) is present but the FFmpeg
    /// library it links against is not beside it in `files/lib/x86_64-linux-gnu`.
    /// Carries the missing library's name pattern. Without it Media Foundation
    /// fails with `0xc000007a` on every mp4, so refuse the tree up front.
    MissingLib(String),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::Missing => write!(f, "no version file: not an installed runtime"),
            VerifyError::Unreadable(e) => write!(f, "version file unreadable: {e}"),
            VerifyError::NotGe(s) => write!(f, "not a GE-Proton runtime: version says {s:?}"),
            VerifyError::MissingLib(l) => write!(
                f,
                "incomplete runtime: files/lib/wine/x86_64-unix/winedmo.so is present but {l} is missing from files/lib/x86_64-linux-gnu"
            ),
        }
    }
}

impl std::error::Error for VerifyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VerifyError::Unreadable(e) => Some(e),
            _ => None,
        }
    }
}

/// Reads `dir/version`, confirms it names a GE-Proton build, and returns the
/// tag (e.g. `"GE-Proton11-6"`).
///
/// The file is whitespace-separated tokens; the token that starts with
/// `GE-Proton` is the tag. If no such token exists, the whole (trimmed) file
/// is returned in [`VerifyError::NotGe`] so the caller can report exactly
/// what was rejected.
pub fn verify_ge(dir: &Path) -> Result<String, VerifyError> {
    let path = dir.join("version");
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(VerifyError::Missing),
        Err(e) => return Err(VerifyError::Unreadable(e)),
    };
    let trimmed = contents.trim();
    let tag = match trimmed
        .split_whitespace()
        .find(|tok| tok.starts_with("GE-Proton"))
    {
        Some(tag) => tag.to_string(),
        None => return Err(VerifyError::NotGe(trimmed.to_string())),
    };
    check_ffmpeg(dir)?;
    Ok(tag)
}

/// `winedmo.so` links `libavformat.so.N` and friends with no RPATH; they live in
/// `files/lib/x86_64-linux-gnu` and are found only through [`runtime_lib_env`].
/// A tree with the demuxer but no `libavformat.so.*` cannot open any mp4.
fn check_ffmpeg(dir: &Path) -> Result<(), VerifyError> {
    let files = dir.join("files");
    if !files.join("lib/wine/x86_64-unix/winedmo.so").exists() {
        return Ok(());
    }
    let has = std::fs::read_dir(files.join("lib/x86_64-linux-gnu"))
        .map(|rd| {
            rd.flatten().any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("libavformat.so.")
            })
        })
        .unwrap_or(false);
    if has {
        Ok(())
    } else {
        Err(VerifyError::MissingLib("libavformat.so.*".to_string()))
    }
}

/// The library environment Proton's `proton` script (`init_wine`) gives Wine:
/// `files/lib/{x86_64,i386}-linux-gnu` prepended to `LD_LIBRARY_PATH` (the
/// inherited value kept after them), `ORIG_LD_LIBRARY_PATH` set to the inherited
/// value unless the host already has one, and `WINEDLLPATH` =
/// `lib/vkd3d:lib/wine[:inherited]`. Pure: the host values are arguments.
/// `runtime` must be absolute.
pub fn runtime_lib_env(
    runtime: &Path,
    inherited_ld: Option<&OsStr>,
    inherited_orig_ld: Option<&OsStr>,
    inherited_dllpath: Option<&OsStr>,
) -> Vec<(String, String)> {
    let lib = runtime.join("files").join("lib");
    let p = |sub: &str| lib.join(sub).to_string_lossy().into_owned();
    let inh = |v: Option<&OsStr>| v.map(|v| v.to_string_lossy().into_owned());
    let mut ld = format!("{}:{}", p("x86_64-linux-gnu"), p("i386-linux-gnu"));
    let ld_in = inh(inherited_ld);
    if let Some(v) = ld_in.as_deref().filter(|v| !v.is_empty()) {
        ld.push(':');
        ld.push_str(v);
    }
    let mut dll = format!("{}:{}", p("vkd3d"), p("wine"));
    if let Some(v) = inh(inherited_dllpath).filter(|v| !v.is_empty()) {
        dll.push(':');
        dll.push_str(&v);
    }
    let mut out = Vec::new();
    if inherited_orig_ld.is_none() {
        out.push((
            "ORIG_LD_LIBRARY_PATH".to_string(),
            ld_in.unwrap_or_default(),
        ));
    }
    out.push(("LD_LIBRARY_PATH".to_string(), ld));
    out.push(("WINEDLLPATH".to_string(), dll));
    out
}

/// [`runtime_lib_env`] with the host's own environment as the inherited values.
pub fn runtime_lib_env_host(runtime: &Path) -> Vec<(String, String)> {
    runtime_lib_env(
        runtime,
        std::env::var_os("LD_LIBRARY_PATH").as_deref(),
        std::env::var_os("ORIG_LD_LIBRARY_PATH").as_deref(),
        std::env::var_os("WINEDLLPATH").as_deref(),
    )
}

/// Orders two `GE-ProtonN-M` tags numerically by `(N, M)`, not lexically:
/// `"GE-Proton11-6"` must sort after `"GE-Proton9-1"`, which string
/// comparison gets backwards. Tags that don't parse as `GE-ProtonN-M` sort
/// below every tag that does, so junk never wins a "newest" selection.
pub fn cmp_tags(a: &str, b: &str) -> Ordering {
    match (parse_tag(a), parse_tag(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

fn parse_tag(tag: &str) -> Option<(u64, u64)> {
    let rest = tag.strip_prefix("GE-Proton")?;
    let (n, m) = rest.split_once('-')?;
    Some((n.parse().ok()?, m.parse().ok()?))
}

/// Lists the tags of every installed, verified GE-Proton runtime under
/// `root.runtimes()`, newest first. Entries that fail [`verify_ge`] — wrong
/// runtime, or a half-extracted directory with no `version` file yet — are
/// silently excluded rather than surfaced as errors, since a stray
/// non-runtime directory there is expected, not exceptional.
pub fn installed(root: &Root) -> io::Result<Vec<String>> {
    Ok(installed_dirs(root)?
        .into_iter()
        .map(|(tag, _)| tag)
        .collect())
}

/// Like [`installed`], but pairs each verified tag with the directory it was
/// actually found in, newest first.
///
/// The pairing is the point: the tag comes from the tree's `version` file
/// while the directory name comes from the release it was installed from, and
/// `vfs-proton list` must not print a path it inferred by re-joining the tag
/// onto `runtimes()` — that assumes the two always agree, and printing a path
/// that does not exist is exactly the failure a `list` command must not have.
pub fn installed_dirs(root: &Root) -> io::Result<Vec<(String, std::path::PathBuf)>> {
    let mut found = Vec::new();
    let entries = match std::fs::read_dir(root.runtimes()) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        // `metadata`, not `DirEntry::file_type`: the latter does not follow
        // symlinks, so a runtime linked in from elsewhere (Steam's
        // `compatibilitytools.d`) was skipped. A dangling link is skipped too.
        if !std::fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        if let Ok(tag) = verify_ge(&path) {
            found.push((tag, path));
        }
    }
    found.sort_by(|(a, _), (b, _)| cmp_tags(a, b).reverse());
    Ok(found)
}

/// The directory of the newest verified runtime under `root.runtimes()`, or
/// `None` when none is installed: the runtime a launch uses, so a host that
/// wants to know "which runtime" asks here rather than re-deriving it from
/// [`installed_dirs`]. The directory is the one found, never re-joined from
/// the tag (see [`installed_dirs`]).
pub fn newest_installed(root: &Root) -> io::Result<Option<std::path::PathBuf>> {
    Ok(installed_dirs(root)?
        .into_iter()
        .next()
        .map(|(_tag, dir)| dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = crate::test_tmp::dir().join(format!("vfs-proton-rt-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn verify_ge_accepts_a_real_ge_version_file() {
        // Exactly the bytes GE-Proton11-6 ships: a build id, a space, the tag.
        let d = tmpdir("ge");
        std::fs::write(d.join("version"), "1787951532 GE-Proton11-6\n").unwrap();
        assert_eq!(verify_ge(&d).unwrap(), "GE-Proton11-6");
    }

    #[test]
    fn verify_ge_rejects_stock_proton() {
        // The failure that matters: PROTONPATH defaults to UMU-Proton, which is
        // stock Valve Proton. Accepting it silently is the whole hazard.
        let d = tmpdir("stock");
        std::fs::write(d.join("version"), "1234567890 proton-9.0-4\n").unwrap();
        match verify_ge(&d) {
            Err(VerifyError::NotGe(s)) => assert!(s.contains("proton-9.0-4")),
            other => panic!("stock Proton must be rejected, got {other:?}"),
        }
    }

    #[test]
    fn verify_ge_rejects_a_missing_version_file() {
        let d = tmpdir("nofile");
        assert!(matches!(verify_ge(&d), Err(VerifyError::Missing)));
    }

    #[test]
    fn verify_ge_tolerates_no_trailing_newline_and_extra_fields() {
        let d = tmpdir("loose");
        std::fs::write(d.join("version"), "1787951532 GE-Proton11-6 extra").unwrap();
        assert_eq!(verify_ge(&d).unwrap(), "GE-Proton11-6");
    }

    fn fake_rt(tag: &str, with_dmo: bool, with_av: bool) -> std::path::PathBuf {
        let d = tmpdir(tag);
        std::fs::write(d.join("version"), "1 GE-Proton11-7\n").unwrap();
        let unix = d.join("files/lib/wine/x86_64-unix");
        let gnu = d.join("files/lib/x86_64-linux-gnu");
        std::fs::create_dir_all(&unix).unwrap();
        std::fs::create_dir_all(&gnu).unwrap();
        if with_dmo {
            std::fs::write(unix.join("winedmo.so"), "").unwrap();
        }
        if with_av {
            std::fs::write(gnu.join("libavformat.so.62.12.100"), "").unwrap();
        }
        d
    }

    #[test]
    fn verify_ge_requires_ffmpeg_beside_winedmo() {
        let d = fake_rt("dmo-noav", true, false);
        match verify_ge(&d) {
            Err(e @ VerifyError::MissingLib(_)) => {
                assert!(e.to_string().contains("libavformat.so"), "{e}")
            }
            other => panic!("expected MissingLib, got {other:?}"),
        }
        assert!(verify_ge(&fake_rt("dmo-av", true, true)).is_ok());
        assert!(verify_ge(&fake_rt("nodmo", false, false)).is_ok());
    }

    #[test]
    fn runtime_lib_env_prepends_and_keeps_inherited() {
        let rt = Path::new("/rt");
        let m: std::collections::HashMap<_, _> = runtime_lib_env(
            rt,
            Some(OsStr::new("/host/lib")),
            None,
            Some(OsStr::new("/host/dll")),
        )
        .into_iter()
        .collect();
        assert_eq!(
            m["LD_LIBRARY_PATH"],
            "/rt/files/lib/x86_64-linux-gnu:/rt/files/lib/i386-linux-gnu:/host/lib"
        );
        assert_eq!(m["ORIG_LD_LIBRARY_PATH"], "/host/lib");
        assert_eq!(
            m["WINEDLLPATH"],
            "/rt/files/lib/vkd3d:/rt/files/lib/wine:/host/dll"
        );
    }

    #[test]
    fn runtime_lib_env_without_host_values_and_with_orig_present() {
        let m: std::collections::HashMap<_, _> =
            runtime_lib_env(Path::new("/rt"), None, None, None)
                .into_iter()
                .collect();
        assert_eq!(
            m["LD_LIBRARY_PATH"],
            "/rt/files/lib/x86_64-linux-gnu:/rt/files/lib/i386-linux-gnu"
        );
        assert_eq!(m["ORIG_LD_LIBRARY_PATH"], "");
        assert_eq!(m["WINEDLLPATH"], "/rt/files/lib/vkd3d:/rt/files/lib/wine");
        let m: std::collections::HashMap<_, _> = runtime_lib_env(
            Path::new("/rt"),
            Some(OsStr::new("/x")),
            Some(OsStr::new("/o")),
            None,
        )
        .into_iter()
        .collect();
        assert!(!m.contains_key("ORIG_LD_LIBRARY_PATH"));
    }

    #[test]
    fn tags_order_numerically_not_lexically() {
        // "GE-Proton11-6" < "GE-Proton9-1" as strings, which would make 9 newer
        // than 11 and pick the wrong default runtime.
        assert_eq!(cmp_tags("GE-Proton11-6", "GE-Proton9-1"), Ordering::Greater);
        assert_eq!(
            cmp_tags("GE-Proton11-10", "GE-Proton11-9"),
            Ordering::Greater
        );
        assert_eq!(cmp_tags("GE-Proton11-6", "GE-Proton11-6"), Ordering::Equal);
    }

    #[test]
    fn installed_lists_only_verified_ge_runtimes_newest_first() {
        let base = tmpdir("installed");
        let root = crate::layout::Root::at(base.clone());
        std::fs::create_dir_all(root.runtimes()).unwrap();
        for (tag, body) in [
            ("GE-Proton11-6", "1 GE-Proton11-6\n"),
            ("GE-Proton9-1", "1 GE-Proton9-1\n"),
            ("junk-dir", "1 proton-9.0-4\n"), // not GE -> excluded
            ("half-extracted", ""),           // no version file -> excluded
        ] {
            let d = root.runtime_dir(tag);
            std::fs::create_dir_all(&d).unwrap();
            if !body.is_empty() {
                std::fs::write(d.join("version"), body).unwrap();
            }
        }
        assert_eq!(
            installed(&root).unwrap(),
            vec!["GE-Proton11-6".to_string(), "GE-Proton9-1".to_string()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_runtime_directory_is_listed_under_its_link() {
        let base = tmpdir("symlinked");
        let root = crate::layout::Root::at(base.join("home"));
        std::fs::create_dir_all(root.runtimes()).unwrap();
        let real = base.join("elsewhere").join("GE-Proton11-7-x86_64");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("version"), "1 GE-Proton11-7\n").unwrap();
        let link = root.runtimes().join("GE-Proton11-7-x86_64");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        std::os::unix::fs::symlink(base.join("gone"), root.runtimes().join("dangling")).unwrap();
        assert_eq!(
            installed_dirs(&root).unwrap(),
            vec![("GE-Proton11-7".to_string(), link)],
            "listed at the link (never canonicalized); the dangling link is skipped"
        );
    }

    #[test]
    fn newest_installed_is_the_first_of_installed_dirs_or_none() {
        let base = tmpdir("newest");
        let root = crate::layout::Root::at(base.join("home"));
        assert_eq!(newest_installed(&root).unwrap(), None, "no runtimes dir");
        for (dir, tag) in [("a", "GE-Proton9-1"), ("b", "GE-Proton11-6")] {
            let d = root.runtimes().join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("version"), format!("1 {tag}\n")).unwrap();
        }
        assert_eq!(
            newest_installed(&root).unwrap(),
            Some(root.runtimes().join("b"))
        );
    }
}
