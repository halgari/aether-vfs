//! Download, verify and extract a GE-Proton release atomically.
//!
//! Three ordering properties carry this module, and they are the reason the
//! code is shaped the way it is rather than the shortest thing that works:
//!
//! 1. **The body is streamed, never buffered.** The real tarball is 533 MB;
//!    reading it into a `Vec` would be a half-gigabyte allocation on a host
//!    that may be running a game. [`ureq`]'s reader goes straight into the
//!    file via [`std::io::copy`], and the digest is computed by reading that
//!    file back in chunks.
//! 2. **A digest mismatch deletes the partial file.** A failed download must
//!    leave nothing behind that a later run could mistake for reusable.
//! 3. **`fs::rename` is the last step.** Extraction happens in a `.tmp-…`
//!    sibling, so a half-extracted tree is never visible under the real
//!    runtime name and a failed install can never look installed.
//!
//! And one gate that is not about ordering: [`crate::runtime::verify_ge`] runs
//! on the *extracted tree*, not only when a runtime is resolved later. Being
//! the right bytes and being GE-Proton are separate claims — a tarball can
//! match its publisher digest perfectly and still be stock Valve Proton, which
//! is exactly what `PROTONPATH` silently falls back to.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufReader, BufWriter, Read};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha512};

use crate::layout::Root;
use crate::release::Release;
use crate::runtime::{verify_ge, VerifyError};

/// Sent on every request: GitHub rejects requests with no `User-Agent`, and
/// the release-asset CDN is fussier still.
const USER_AGENT: &str = "aether-vfs (vfs-proton)";

/// Cap on the sha512sum response. The real asset is 158 bytes; anything near
/// this is an error page, and reading an unbounded body into a `String` is how
/// a bad server exhausts client memory.
const MAX_DIGEST_BODY: u64 = 64 * 1024;

/// Buffer used for both the download and the re-read that hashes it.
const CHUNK: usize = 256 * 1024;

/// A runtime that is on disk under its real name and has passed the
/// GE-Proton gate.
///
/// `fresh` is false when the runtime was already installed and verified, so a
/// caller can tell an idempotent no-op from a 533 MB download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub tag: String,
    pub dir: PathBuf,
    pub fresh: bool,
}

/// Why an install failed.
#[derive(Debug)]
pub enum InstallError {
    /// A filesystem operation failed.
    Io(io::Error),
    /// The request failed, or returned a non-success status.
    Http(String),
    /// The downloaded bytes did not hash to the publisher's digest. The
    /// partial file has already been deleted when this is returned.
    Digest { expected: String, actual: String },
    /// The `.sha512sum` body was not `<128 hex>  <filename>`. Carries the
    /// offending text (truncated) so a 404 HTML page is recognisable.
    BadSha512Line(String),
    /// The archive could not be read, or did not have exactly one top-level
    /// directory.
    Archive(String),
    /// The extracted tree is not GE-Proton. Never a warning: `PROTONPATH`
    /// defaults to stock Valve Proton, so a soft failure here wastes a day.
    NotGe(String),
    /// A tar member, a link target, or the release tag itself would have
    /// resolved outside the directory it was being unpacked into.
    Traversal(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::Io(e) => write!(f, "io error: {e}"),
            InstallError::Http(e) => write!(f, "http error: {e}"),
            InstallError::Digest { expected, actual } => {
                write!(f, "sha512 mismatch: expected {expected}, got {actual}")
            }
            InstallError::BadSha512Line(s) => write!(f, "malformed sha512sum line: {s:?}"),
            InstallError::Archive(e) => write!(f, "bad archive: {e}"),
            InstallError::NotGe(s) => write!(f, "not a GE-Proton runtime: {s}"),
            InstallError::Traversal(s) => write!(f, "refused path traversal: {s}"),
        }
    }
}

impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            InstallError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for InstallError {
    fn from(e: io::Error) -> Self {
        InstallError::Io(e)
    }
}

/// Parses a publisher `.sha512sum` body into a lowercase 128-hex digest.
///
/// GE ships exactly one line, `<128 hex>  <filename>` (two spaces). Only the
/// first non-blank line's first token is used, and it must be exactly 128 hex
/// characters. That length-and-alphabet check is the whole point: without it a
/// truncated body or a 404 HTML page becomes an "expected digest" that
/// verification then compares against nonsense, and the comparison passes or
/// fails for reasons unrelated to the bytes on disk.
pub fn parse_sha512sum(body: &str) -> Result<String, InstallError> {
    let bad = || InstallError::BadSha512Line(body.chars().take(120).collect::<String>());
    let line = body.lines().find(|l| !l.trim().is_empty()).ok_or_else(bad)?;
    let token = line.split_whitespace().next().ok_or_else(bad)?;
    if token.len() != 128 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad());
    }
    Ok(token.to_ascii_lowercase())
}

/// Hashes `path` with SHA-512 and compares against `expected_hex`.
///
/// Reads in [`CHUNK`]-sized pieces rather than slurping the file: this runs on
/// a 533 MB tarball.
pub fn verify_digest(path: &Path, expected_hex: &str) -> Result<(), InstallError> {
    let mut hasher = Sha512::new();
    let mut reader = BufReader::with_capacity(CHUNK, fs::File::open(path)?);
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = hex_lower(&hasher.finalize());
    let expected = expected_hex.to_ascii_lowercase();
    if actual == expected {
        Ok(())
    } else {
        Err(InstallError::Digest { expected, actual })
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing to a String is infallible.
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Unpacks `archive` (a gzipped tar) into `into` and returns the single
/// top-level directory it created.
///
/// Traversal is refused **explicitly**, not delegated to the `tar` crate.
/// `tar::Entry::unpack(dst)` passes `target_base: None`, which switches off
/// that crate's own containment checks entirely, so relying on its defaults
/// here would mean relying on nothing. Every member is therefore validated and
/// then written by this function:
///
/// * a member path may contain only normal (or `.`) components — no `..`, no
///   root, no Windows prefix;
/// * a symlink target is resolved *lexically* against the link's own parent
///   and must land inside the tree. Lexically, not via `canonicalize`, because
///   the tree is still being created; and containment rather than "no `..`"
///   because GE-Proton's real tree contains legitimate relative symlinks whose
///   targets climb before descending;
/// * a hard link target is resolved the same way against the archive root,
///   which is what tar hard-link names are relative to.
///
/// The symlink rule is what closes the interesting hole: a contained member
/// path is not enough on its own, because a symlink pointing outside plus a
/// later member written *through* it lands outside the tree while every
/// member path looks innocent.
pub fn extract_tar_gz(archive: &Path, into: &Path) -> Result<PathBuf, InstallError> {
    let file = fs::File::open(archive)?;
    let decoder = flate2::read::GzDecoder::new(BufReader::with_capacity(CHUNK, file));
    let mut tar = tar::Archive::new(decoder);

    let mut top: Option<OsString> = None;
    let entries = tar
        .entries()
        .map_err(|e| InstallError::Archive(e.to_string()))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| InstallError::Archive(e.to_string()))?;
        let kind = entry.header().entry_type();

        // Metadata-only members carry no path of their own; the tar crate
        // folds them into the entry that follows.
        if kind.is_pax_global_extensions()
            || kind.is_pax_local_extensions()
            || kind.is_gnu_longname()
            || kind.is_gnu_longlink()
        {
            continue;
        }

        let raw = entry
            .path()
            .map_err(|e| InstallError::Archive(e.to_string()))?
            .into_owned();
        let rel = safe_member_path(&raw)?;
        if rel.as_os_str().is_empty() {
            continue;
        }

        // One top-level directory per archive, and it names the runtime.
        let first = rel
            .components()
            .next()
            .and_then(|c| match c {
                Component::Normal(s) => Some(s.to_os_string()),
                _ => None,
            })
            .ok_or_else(|| {
                InstallError::Archive(format!("member {} has no top-level name", raw.display()))
            })?;
        match &top {
            None => top = Some(first),
            Some(seen) if *seen == first => {}
            Some(seen) => {
                return Err(InstallError::Archive(format!(
                    "archive has more than one top-level entry: {} and {}",
                    Path::new(seen).display(),
                    Path::new(&first).display()
                )));
            }
        }

        let dst = into.join(&rel);

        if kind.is_dir() {
            fs::create_dir_all(&dst)?;
            continue;
        }

        if kind.is_symlink() || kind.is_hard_link() {
            let target = entry
                .link_name()
                .map_err(|e| InstallError::Archive(e.to_string()))?
                .ok_or_else(|| {
                    InstallError::Archive(format!("link {} has no target", raw.display()))
                })?
                .into_owned();
            // A symlink target is relative to the link's own directory; a tar
            // hard-link name is relative to the archive root.
            let anchor: &Path = if kind.is_symlink() {
                rel.parent().unwrap_or(Path::new(""))
            } else {
                Path::new("")
            };
            let resolved = resolve_lexically(anchor, &target).ok_or_else(|| {
                InstallError::Traversal(format!(
                    "link {} targets {} outside the archive tree",
                    raw.display(),
                    target.display()
                ))
            })?;
            create_parent(&dst)?;
            if kind.is_symlink() {
                symlink(&target, &dst)?;
            } else {
                fs::hard_link(into.join(&resolved), &dst)?;
            }
            continue;
        }

        if !(kind.is_file() || kind == tar::EntryType::Continuous) {
            // Device nodes and fifos have no business in a Proton tarball, and
            // creating them is not something this crate should be able to do.
            continue;
        }

        create_parent(&dst)?;
        let mode = entry.header().mode().unwrap_or(0o644);
        let mut out = BufWriter::with_capacity(CHUNK, fs::File::create(&dst)?);
        io::copy(&mut entry, &mut out)?;
        let out = out.into_inner().map_err(|e| e.into_error())?;
        apply_mode(&out, mode)?;
    }

    let top = top.ok_or_else(|| InstallError::Archive("archive is empty".to_string()))?;
    let dir = into.join(&top);
    // A single *file* at the top level is not a runtime.
    if !dir.is_dir() {
        return Err(InstallError::Archive(format!(
            "top-level entry {} is not a directory",
            Path::new(&top).display()
        )));
    }
    Ok(dir)
}

/// Accepts a tar member path only if every component is normal (or `.`).
/// Rejects `..`, absolute paths, and Windows prefixes, which is the check the
/// brief calls out and the one `Entry::unpack` does not perform.
fn safe_member_path(raw: &Path) -> Result<PathBuf, InstallError> {
    let mut out = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(InstallError::Traversal(format!(
                    "member {} escapes the target directory",
                    raw.display()
                )));
            }
        }
    }
    Ok(out)
}

/// Resolves `target` against `anchor` (both relative to the extraction root)
/// without touching the filesystem, returning `None` if the result climbs
/// above the root or is absolute.
fn resolve_lexically(anchor: &Path, target: &Path) -> Option<PathBuf> {
    let mut stack: Vec<OsString> = Vec::new();
    for component in anchor.components() {
        match component {
            Component::Normal(s) => stack.push(s.to_os_string()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    for component in target.components() {
        match component {
            Component::Normal(s) => stack.push(s.to_os_string()),
            Component::CurDir => {}
            // Popping an empty stack means the target reached above the root.
            Component::ParentDir => {
                stack.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(stack.into_iter().collect())
}

fn create_parent(dst: &Path) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
fn apply_mode(file: &fs::File, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // Keep the archive's executable bits — Proton is full of binaries — but
    // floor the owner bits, so a header with mode 0 (which `tar::Builder`
    // happily produces) cannot yield a file its owner cannot read.
    let mode = (mode & 0o777) | 0o600;
    file.set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn apply_mode(_file: &fs::File, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, dst: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, dst)
}

#[cfg(windows)]
fn symlink(target: &Path, dst: &Path) -> io::Result<()> {
    // Unprivileged Windows cannot create symlinks, and a Linux Proton tree is
    // not runnable there anyway; the error is surfaced rather than swallowed so
    // nothing pretends the tree is complete.
    std::os::windows::fs::symlink_file(target, dst)
}

/// Downloads, verifies and installs `rel` under `root`, returning the
/// installed runtime.
///
/// The sequence is the substance of this function; see the module docs for
/// why. In short: reuse an already-verified install unless `force`; fetch and
/// parse the digest *before* the 533 MB body, so a broken digest asset costs
/// nothing; stream the body to a `.partial`; verify and delete the `.partial`
/// on mismatch; extract into a `.tmp-…` sibling; gate on
/// [`verify_ge`]; and only then rename into place.
pub fn install_release(
    root: &Root,
    rel: &Release,
    agent: &ureq::Agent,
    force: bool,
) -> Result<Installed, InstallError> {
    // The tag reaches us from a GitHub release name and may reach us from a
    // CLI argument, so it is validated before it is ever joined onto a path —
    // and before anything is created on disk.
    let final_dir = root
        .try_runtime_dir(&rel.tag)
        .map_err(|e| InstallError::Traversal(e.to_string()))?;

    // 1. Idempotence: an install that is already there and already GE is a
    //    no-op, not a re-download.
    if !force {
        if let Ok(tag) = verify_ge(&final_dir) {
            return Ok(Installed {
                tag,
                dir: final_dir,
                fresh: false,
            });
        }
    }

    let runtimes = root.runtimes();
    let downloads = root.downloads();
    fs::create_dir_all(&runtimes)?;
    fs::create_dir_all(&downloads)?;

    // 2. The digest first: it is 158 bytes, and fetching it before the tarball
    //    means a missing or malformed digest asset never costs a download.
    let expected = parse_sha512sum(&get_text(agent, &rel.digest_url)?)?;

    // 3. Stream the body to a `.partial`. Never buffered: the real body is
    //    533,700,853 bytes.
    let partial = downloads.join(format!("{}.tar.gz.partial", rel.tag));
    download_to(agent, &rel.tarball_url, &partial)?;

    // 4. Verify, and on mismatch delete the partial so a later run cannot
    //    mistake it for a resumable or reusable download.
    if let Err(e) = verify_digest(&partial, &expected) {
        let _ = fs::remove_file(&partial);
        return Err(e);
    }

    // 5. Extract into a sibling temp directory, never into the real name.
    let tmp = runtimes.join(format!(".tmp-{}-{}", rel.tag, std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;

    let installed = install_from_partial(rel, &partial, &tmp, &final_dir, &runtimes);

    // 8. Clean up regardless of outcome, and unconditionally. A leftover
    //    `.tmp-…` is not dangerous (it fails `verify_ge`, so `installed()`
    //    skips it) but it is 1.4 GB, and a surviving `.partial` is 533 MB that
    //    nothing will ever reuse — there is no resume path, so keeping it after
    //    a failure only invites a later run to trust bytes it did not verify.
    let _ = fs::remove_dir_all(&tmp);
    let _ = fs::remove_file(&partial);
    installed
}

/// Steps 5-7: extract, gate on GE, then rename. Split out so
/// [`install_release`] can clean up the temp tree on every path without
/// repeating the teardown at each early return.
fn install_from_partial(
    rel: &Release,
    partial: &Path,
    tmp: &Path,
    final_dir: &Path,
    runtimes: &Path,
) -> Result<Installed, InstallError> {
    let top = extract_tar_gz(partial, tmp)?;

    // 6. Right bytes and right runtime are different claims. A tarball can
    //    match its publisher digest and still be stock Valve Proton, so the GE
    //    gate runs on the extracted tree too, not only at resolution time.
    match verify_ge(&top) {
        Ok(_) => {}
        Err(VerifyError::NotGe(s)) => return Err(InstallError::NotGe(s)),
        Err(VerifyError::Missing) => {
            return Err(InstallError::NotGe(format!(
                "{} has no version file",
                top.display()
            )))
        }
        Err(VerifyError::Unreadable(e)) => return Err(InstallError::Io(e)),
    }

    // 7. Rename last. Until this line nothing under the real runtime name has
    //    changed, so every failure above leaves the previous state intact and a
    //    half-extracted tree is never visible as an install.
    //
    //    An existing directory is moved aside rather than deleted, and restored
    //    if the rename fails, so `force` cannot destroy a working runtime in
    //    exchange for a broken one.
    let displaced = if final_dir.exists() {
        let aside = runtimes.join(format!(".old-{}-{}", rel.tag, std::process::id()));
        let _ = fs::remove_dir_all(&aside);
        fs::rename(final_dir, &aside)?;
        Some(aside)
    } else {
        None
    };
    match fs::rename(&top, final_dir) {
        Ok(()) => {
            if let Some(aside) = displaced {
                let _ = fs::remove_dir_all(aside);
            }
        }
        Err(e) => {
            if let Some(aside) = displaced {
                let _ = fs::rename(&aside, final_dir);
            }
            return Err(InstallError::Io(e));
        }
    }

    Ok(Installed {
        tag: rel.tag.clone(),
        dir: final_dir.to_path_buf(),
        fresh: true,
    })
}

/// GETs a small text body, bounded by [`MAX_DIGEST_BODY`].
fn get_text(agent: &ureq::Agent, url: &str) -> Result<String, InstallError> {
    let mut res = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| InstallError::Http(format!("GET {url}: {e}")))?;
    if !res.status().is_success() {
        return Err(InstallError::Http(format!("GET {url}: {}", res.status())));
    }
    res.body_mut()
        .with_config()
        .limit(MAX_DIGEST_BODY)
        .read_to_string()
        .map_err(|e| InstallError::Http(format!("GET {url}: {e}")))
}

/// Streams `url` into `dest`.
///
/// `as_reader` plus [`io::copy`] is the whole point: the body is 533 MB and
/// must never exist in memory. The reader is deliberately unlimited — a size
/// cap here could only reject a legitimate download whose length differs from
/// what the API reported, and it is the digest, not the length, that decides
/// whether these bytes are trusted.
fn download_to(agent: &ureq::Agent, url: &str, dest: &Path) -> Result<(), InstallError> {
    let mut res = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| InstallError::Http(format!("GET {url}: {e}")))?;
    if !res.status().is_success() {
        return Err(InstallError::Http(format!("GET {url}: {}", res.status())));
    }
    let file = fs::File::create(dest)?;
    let mut writer = BufWriter::with_capacity(CHUNK, file);
    let mut reader = res.body_mut().as_reader();
    io::copy(&mut reader, &mut writer)?;
    let file = writer.into_inner().map_err(|e| e.into_error())?;
    // The digest is computed by reading this file back, so it has to be on
    // disk in full first.
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_publisher_sha512sum_line() {
        // Exactly the format GE ships: "<128 hex>  <filename>".
        let line = "543e3af57bb138b1be5a5b98bba4d39ca59340bfa34ec8c12144f3e16d7434ed\
75bd7a68eafc228b16695884629595af0905156e5227c1898f93cdbc92cb5fcb  GE-Proton11-6-x86_64.tar.gz";
        assert_eq!(parse_sha512sum(line).unwrap().len(), 128);
        assert!(parse_sha512sum(line).unwrap().starts_with("543e3af5"));
    }

    #[test]
    fn rejects_a_sha512sum_that_is_not_128_hex_chars() {
        // A truncated or HTML error page must not be accepted as a digest, or
        // verification silently compares against nonsense.
        for bad in ["", "deadbeef  f.tar.gz", "<html>404</html>", "zzzz  f"] {
            assert!(parse_sha512sum(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn verify_digest_accepts_the_true_hash_and_rejects_a_wrong_one() {
        let p = std::env::temp_dir()
            .join(format!("vfs-proton-dg-{}.bin", std::process::id()));
        std::fs::write(&p, b"abc").unwrap();
        // Known SHA-512 of "abc".
        let want = "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f";
        verify_digest(&p, want).unwrap();
        match verify_digest(&p, &"0".repeat(128)) {
            Err(InstallError::Digest { actual, .. }) => assert_eq!(actual, want),
            other => panic!("a wrong digest must fail, got {other:?}"),
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn extract_refuses_an_archive_with_a_traversing_member() {
        // A tar entry named ../escaped would write outside the target directory.
        // Build such an archive and require refusal.
        let dir = std::env::temp_dir()
            .join(format!("vfs-proton-evil-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("evil.tar.gz");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut b = tar::Builder::new(enc);
            let mut h = tar::Header::new_gnu();
            // DEVIATION from the brief, forced by tar 0.4.46: `set_path` itself
            // errors with "paths in archives must not have `..`", so the brief's
            // `h.set_path("../escaped.txt").unwrap()` panics before
            // `extract_tar_gz` is ever called and the test can never exercise
            // the refusal it exists to prove. A real attacker writes the header
            // bytes, not `set_path`, so the name field is written directly here
            // to produce the archive the brief describes.
            let name = b"../escaped.txt";
            h.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
            h.set_size(3);
            h.set_cksum();
            b.append(&h, &b"pwn"[..]).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        let into = dir.join("into");
        std::fs::create_dir_all(&into).unwrap();
        assert!(
            matches!(extract_tar_gz(&archive, &into), Err(InstallError::Traversal(_))),
            "a traversing member must be refused"
        );
        assert!(!dir.join("escaped.txt").exists(), "nothing may be written outside");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_returns_the_single_top_level_directory() {
        let dir = std::env::temp_dir()
            .join(format!("vfs-proton-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("ok.tar.gz");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut b = tar::Builder::new(enc);
            let mut h = tar::Header::new_gnu();
            h.set_path("GE-Proton11-6-x86_64/version").unwrap();
            h.set_size(25);
            h.set_cksum();
            b.append(&h, &b"1787951532 GE-Proton11-6\n"[..]).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        let into = dir.join("into");
        std::fs::create_dir_all(&into).unwrap();
        let top = extract_tar_gz(&archive, &into).unwrap();
        assert_eq!(top.file_name().unwrap(), "GE-Proton11-6-x86_64");
        assert_eq!(
            std::fs::read_to_string(top.join("version")).unwrap().trim(),
            "1787951532 GE-Proton11-6"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- Tests beyond the brief's five ----------------------------------
    //
    // `tar::Entry::unpack(dst)` passes `target_base: None`, which switches off
    // the tar crate's own link-target validation entirely. So a symlink whose
    // *target* escapes is not covered by checking member paths alone: a later
    // member written through that symlink lands outside the tree. Both link
    // kinds are therefore checked here.

    fn write_link_archive(archive: &Path, kind: tar::EntryType, link_target: &str) {
        let f = std::fs::File::create(archive).unwrap();
        let enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
        let mut b = tar::Builder::new(enc);
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(kind);
        h.set_path("GE-Proton11-6-x86_64/escape").unwrap();
        h.set_link_name(link_target).unwrap();
        h.set_size(0);
        h.set_cksum();
        b.append(&h, &b""[..]).unwrap();
        b.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn extract_refuses_a_symlink_whose_target_escapes() {
        let dir = std::env::temp_dir()
            .join(format!("vfs-proton-symesc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let into = dir.join("into");
        std::fs::create_dir_all(&into).unwrap();

        for target in ["../../../../etc/passwd", "/etc/passwd"] {
            let archive = dir.join("sym.tar.gz");
            write_link_archive(&archive, tar::EntryType::Symlink, target);
            assert!(
                matches!(
                    extract_tar_gz(&archive, &into),
                    Err(InstallError::Traversal(_))
                ),
                "symlink target {target:?} must be refused"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_refuses_a_hard_link_whose_target_escapes() {
        let dir = std::env::temp_dir()
            .join(format!("vfs-proton-hardesc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let into = dir.join("into");
        std::fs::create_dir_all(&into).unwrap();

        let archive = dir.join("hard.tar.gz");
        write_link_archive(&archive, tar::EntryType::Link, "../../../../etc/passwd");
        assert!(
            matches!(
                extract_tar_gz(&archive, &into),
                Err(InstallError::Traversal(_))
            ),
            "an escaping hard link must be refused"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_allows_a_relative_symlink_that_stays_inside_the_tree() {
        // GE-Proton's real tree contains relative symlinks with `..` in the
        // target (e.g. `files/lib/x/y -> ../../z`). Refusing every `..` would
        // break the real extraction, so containment, not the literal `..`, is
        // the rule.
        let dir = std::env::temp_dir()
            .join(format!("vfs-proton-syminside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("sym.tar.gz");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut b = tar::Builder::new(enc);
            let mut h = tar::Header::new_gnu();
            h.set_path("GE-Proton11-6-x86_64/version").unwrap();
            h.set_size(25);
            h.set_cksum();
            b.append(&h, &b"1787951532 GE-Proton11-6\n"[..]).unwrap();
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_path("GE-Proton11-6-x86_64/files/lib/alias").unwrap();
            h.set_link_name("../../version").unwrap();
            h.set_size(0);
            h.set_cksum();
            b.append(&h, &b""[..]).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        let into = dir.join("into");
        std::fs::create_dir_all(&into).unwrap();
        // Creating a symlink needs privilege on Windows, so tolerate an Io
        // error there; what must never happen is a Traversal refusal.
        match extract_tar_gz(&archive, &into) {
            Ok(top) => assert_eq!(top.file_name().unwrap(), "GE-Proton11-6-x86_64"),
            Err(InstallError::Io(_)) if cfg!(windows) => {}
            other => panic!("an inside-the-tree symlink must not be refused: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_then_verify_ge_rejects_a_stock_proton_tarball() {
        // Being the right bytes and being GE-Proton are separate claims: a
        // tarball can match its publisher digest and still be stock Valve
        // Proton, which PROTONPATH would happily use. install_release runs
        // verify_ge on the extracted tree for exactly this reason.
        let dir = std::env::temp_dir()
            .join(format!("vfs-proton-stock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("stock.tar.gz");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut b = tar::Builder::new(enc);
            let mut h = tar::Header::new_gnu();
            h.set_path("proton-9.0-4/version").unwrap();
            h.set_size(22);
            h.set_cksum();
            b.append(&h, &b"1234567890 proton-9.0\n"[..]).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        let into = dir.join("into");
        std::fs::create_dir_all(&into).unwrap();
        let top = extract_tar_gz(&archive, &into).unwrap();
        assert!(
            matches!(crate::runtime::verify_ge(&top), Err(crate::runtime::VerifyError::NotGe(_))),
            "a stock Proton tree must not pass the GE gate"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_already_installed_verified_runtime_short_circuits_without_downloading() {
        // Both URLs point at loopback port 1, so this test cannot reach the
        // network by construction: if the short-circuit regressed, the test
        // fails with a connection-refused Http error instead of downloading
        // 533 MB.
        let base = std::env::temp_dir()
            .join(format!("vfs-proton-idem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = Root::at(base.clone());
        let dir = root.runtime_dir("GE-Proton11-6");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("version"), "1787951532 GE-Proton11-6\n").unwrap();

        let rel = Release {
            tag: "GE-Proton11-6".to_string(),
            tarball_url: "http://127.0.0.1:1/unreachable.tar.gz".to_string(),
            digest_url: "http://127.0.0.1:1/unreachable.sha512sum".to_string(),
            size: 533_700_853,
        };
        let agent = ureq::Agent::new_with_defaults();
        let got = install_release(&root, &rel, &agent, false).unwrap();
        assert!(!got.fresh, "an installed, verified runtime must not be re-downloaded");
        assert_eq!(got.tag, "GE-Proton11-6");
        assert_eq!(got.dir, dir);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_tag_that_would_escape_the_runtimes_directory_is_refused_before_any_io() {
        let base = std::env::temp_dir()
            .join(format!("vfs-proton-badtag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = Root::at(base.clone());
        let rel = Release {
            tag: "../../escaped".to_string(),
            tarball_url: "http://127.0.0.1:1/unreachable.tar.gz".to_string(),
            digest_url: "http://127.0.0.1:1/unreachable.sha512sum".to_string(),
            size: 1,
        };
        let agent = ureq::Agent::new_with_defaults();
        assert!(matches!(
            install_release(&root, &rel, &agent, false),
            Err(InstallError::Traversal(_))
        ));
        assert!(!base.exists(), "a refused tag must not create any directory");
    }
}
