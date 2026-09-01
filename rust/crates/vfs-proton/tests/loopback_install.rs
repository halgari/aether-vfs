//! End-to-end tests for [`install_release`]'s step ordering, over loopback.
//!
//! # Why a server at all
//!
//! Every other test in this crate exercises a piece — `parse_sha512sum`,
//! `verify_digest`, `extract_tar_gz`, `verify_ge` — and the pieces were all
//! covered. The *sequence* was not, and the sequence is where the two
//! properties that make this an installer rather than a downloader live:
//!
//! 1. **A digest mismatch deletes the partial file.** Nothing is left behind
//!    that a later run could mistake for reusable, because there is no resume
//!    path and unverified bytes must never be trusted later.
//! 2. **`fs::rename` is the last step.** A failed install — bad archive, right
//!    bytes but the wrong runtime — leaves *nothing* under the real runtime
//!    name, so a broken install can never look installed.
//!
//! Neither can be observed without actually running `install_release` from end
//! to end, and running it requires an HTTP server. So there is one here: a
//! [`TcpListener`] on `127.0.0.1:0`, ~40 lines, serving two fixed bodies. It
//! honours the crate's no-network rule exactly — loopback, an ephemeral port,
//! no name resolution, no route off the machine, and nothing to download but
//! the few hundred bytes the test itself built.
//!
//! The server also **counts requests**, which is how idempotence is proven as
//! a fact rather than an inference: the second `install_release` must leave the
//! count untouched.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use sha2::{Digest as _, Sha512};
use vfs_proton::{install_release, InstallError, Release, Root};

// ─── the loopback server ─────────────────────────────────────────────────────

struct Server {
    base: String,
    requests: Arc<AtomicUsize>,
}

impl Server {
    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// A [`Release`] whose two URLs point at this server. The `.sha512sum` and
    /// `.tar.gz` suffixes are what the handler dispatches on, and they are the
    /// real upstream asset names.
    fn release(&self, tag: &str, size: u64) -> Release {
        Release {
            tag: tag.to_string(),
            tarball_url: format!("{}/{tag}-x86_64.tar.gz", self.base),
            digest_url: format!("{}/{tag}-x86_64.sha512sum", self.base),
            size,
        }
    }
}

/// Serves `digest` for any request whose target ends in `.sha512sum` and
/// `tarball` for anything else, counting every request it answers.
fn serve(digest: String, tarball: Vec<u8>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);

    // Detached on purpose: the thread lives as long as the test binary, and the
    // listener dies with it. Joining would mean shutting the listener down,
    // which is more machinery than a fixed-body server needs.
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(target) = read_request_target(&mut stream) else {
                continue;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let body: &[u8] = if target.ends_with(".sha512sum") {
                digest.as_bytes()
            } else {
                &tarball
            };
            let head = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/octet-stream\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
        }
    });

    Server {
        base: format!("http://127.0.0.1:{port}"),
        requests,
    }
}

/// Reads the request line and drains the headers, returning the request
/// target. These are GETs, so there is no body to worry about consuming.
fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    request_line.split_whitespace().nth(1).map(str::to_string)
}

// ─── fixtures ────────────────────────────────────────────────────────────────

/// Builds a gzipped tar in memory. No symlinks: this test runs on Windows too,
/// where creating one needs privilege.
fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    for (name, body) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_path(name).expect("member path");
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, *body).expect("append");
    }
    builder
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gzip")
}

/// A minimal but genuine GE-Proton tree: the `version` file the GE gate reads,
/// plus the `files/bin/wine` the real tarball carries.
fn ge_tarball(tag: &str) -> Vec<u8> {
    let version = format!("1787951532 {tag}\n");
    tar_gz(&[
        (
            &format!("{tag}-x86_64/version"),
            version.as_bytes() as &[u8],
        ),
        (&format!("{tag}-x86_64/files/bin/wine"), b"#!/bin/sh\n"),
    ])
}

/// The publisher's `.sha512sum` body for `body`: `<128 hex><two spaces><name>`.
fn sha512sum_body(body: &[u8], name: &str) -> String {
    let mut hasher = Sha512::new();
    hasher.update(body);
    let hex = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    format!("{hex}  {name}\n")
}

fn base_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vfs-proton-loop-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Names of the entries directly under `dir`; empty when `dir` does not exist.
fn entries(dir: &Path) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

// ─── the tests ───────────────────────────────────────────────────────────────

#[test]
fn a_full_install_over_loopback_is_verified_extracted_and_then_idempotent() {
    let tag = "GE-Proton11-6";
    let tarball = ge_tarball(tag);
    let server = serve(
        sha512sum_body(&tarball, &format!("{tag}-x86_64.tar.gz")),
        tarball.clone(),
    );

    let base = base_dir("ok");
    let root = Root::at(base.clone());
    let release = server.release(tag, tarball.len() as u64);
    let agent = ureq::Agent::new_with_defaults();

    let installed = install_release(&root, &release, &agent, false).expect("install");
    assert!(installed.fresh, "the first install must be a real download");
    assert_eq!(installed.tag, tag);
    assert_eq!(installed.dir, root.runtime_dir(tag));
    assert_eq!(
        std::fs::read_to_string(installed.dir.join("version"))
            .expect("version file")
            .trim(),
        format!("1787951532 {tag}")
    );
    assert!(
        installed.dir.join("files/bin/wine").exists(),
        "the extracted tree must carry files/bin/wine"
    );
    // Step 8: the 533 MB `.partial` does not survive a *successful* install
    // either — there is no resume path, so keeping it only wastes the disk.
    assert_eq!(
        entries(&root.downloads()),
        Vec::<String>::new(),
        "no partial may survive a completed install"
    );
    let after_first = server.requests();
    assert_eq!(after_first, 2, "one digest request and one tarball request");

    // Idempotence as a fact, not an inference: the server counter must not move.
    let again = install_release(&root, &release, &agent, false).expect("second install");
    assert!(!again.fresh, "an installed, verified runtime is a no-op");
    assert_eq!(again.dir, installed.dir);
    assert_eq!(
        server.requests(),
        after_first,
        "a no-op install must perform no request at all"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_digest_mismatch_deletes_the_partial_and_installs_nothing() {
    let tag = "GE-Proton11-6";
    let tarball = ge_tarball(tag);
    // The digest advertised is of *other* bytes, which is exactly what a
    // corrupted or truncated download looks like from the client's side.
    let server = serve(
        sha512sum_body(
            b"these are not the bytes served",
            "GE-Proton11-6-x86_64.tar.gz",
        ),
        tarball.clone(),
    );

    let base = base_dir("digest");
    let root = Root::at(base.clone());
    let release = server.release(tag, tarball.len() as u64);
    let agent = ureq::Agent::new_with_defaults();

    match install_release(&root, &release, &agent, false) {
        Err(InstallError::Digest { expected, actual }) => {
            // Both hashes are named, so the failure is diagnosable rather than
            // merely reported.
            assert_eq!(expected.len(), 128);
            assert_eq!(actual.len(), 128);
            assert_ne!(expected, actual);
        }
        other => panic!("a mismatched digest must fail with Digest, got {other:?}"),
    }
    assert_eq!(
        entries(&root.downloads()),
        Vec::<String>::new(),
        "the unverified partial must be deleted, not left for a later run to trust"
    );
    assert!(
        !root.try_runtime_dir(tag).expect("valid tag").exists(),
        "a failed download must create no runtime directory"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_corrupt_archive_with_a_valid_digest_leaves_nothing_under_the_real_name() {
    let tag = "GE-Proton11-6";
    // The right bytes by the publisher's own reckoning, and still not an
    // archive. Extraction is step 5; the rename is step 7. Nothing may appear
    // under the runtime name.
    let body = b"\x1f\x8b truncated, not a valid gzip stream".to_vec();
    let server = serve(
        sha512sum_body(&body, "GE-Proton11-6-x86_64.tar.gz"),
        body.clone(),
    );

    let base = base_dir("corrupt");
    let root = Root::at(base.clone());
    let release = server.release(tag, body.len() as u64);
    let agent = ureq::Agent::new_with_defaults();

    let err = install_release(&root, &release, &agent, false)
        .expect_err("a corrupt archive must not install");
    assert!(
        matches!(err, InstallError::Archive(_) | InstallError::Io(_)),
        "expected an archive failure, got {err:?}"
    );
    assert!(
        !root.try_runtime_dir(tag).expect("valid tag").exists(),
        "fs::rename is the last step: a failed extraction leaves no runtime directory"
    );
    assert_eq!(
        entries(&root.runtimes()),
        Vec::<String>::new(),
        "the .tmp- extraction directory must be cleaned up too"
    );
    assert_eq!(entries(&root.downloads()), Vec::<String>::new());

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn stock_proton_with_a_valid_digest_is_refused_before_the_rename() {
    // The failure mode the whole crate exists for: bytes that match the
    // publisher digest perfectly and are still not GE-Proton. `PROTONPATH`
    // silently falls back to stock Valve Proton, so this must be an error, and
    // — because the GE gate runs before step 7 — must leave nothing installed.
    let tag = "GE-Proton11-6";
    let tarball = tar_gz(&[("GE-Proton11-6-x86_64/version", b"1234567890 proton-9.0-4\n")]);
    let server = serve(
        sha512sum_body(&tarball, "GE-Proton11-6-x86_64.tar.gz"),
        tarball.clone(),
    );

    let base = base_dir("stock");
    let root = Root::at(base.clone());
    let release = server.release(tag, tarball.len() as u64);
    let agent = ureq::Agent::new_with_defaults();

    match install_release(&root, &release, &agent, false) {
        Err(InstallError::NotGe(reported)) => {
            assert!(
                reported.contains("proton-9.0-4"),
                "the rejection must name what it rejected, got {reported:?}"
            );
        }
        other => panic!("stock Proton must be refused, got {other:?}"),
    }
    assert!(
        !root.try_runtime_dir(tag).expect("valid tag").exists(),
        "a non-GE tree must never reach the real runtime name"
    );
    assert_eq!(entries(&root.runtimes()), Vec::<String>::new());

    let _ = std::fs::remove_dir_all(&base);
}
