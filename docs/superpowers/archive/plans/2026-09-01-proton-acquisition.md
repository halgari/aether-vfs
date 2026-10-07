# GE-Proton acquisition — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A command that downloads, verifies and installs GE-Proton into
aether-vfs's own directory, so a host needs no Steam, no system packages, and no
manual setup.

**Architecture:** A new `vfs-proton` crate whose *acquisition* half is portable
(so its logic is covered by the Windows CI job too, which is the thicker of the
two) and whose launch half arrives in a later increment. A `vfs-proton` binary
exposes `install` / `list` / `path`. Downloads land in a temp file, are verified
against the publisher's `.sha512sum`, extract to a temp directory, and only then
get renamed into place — so a failed or partial install never leaves something
that looks installed.

**Tech Stack:** Rust; `ureq` 3.4 (brings `rustls` + `ring` + webpki-roots in its
defaults), `sha2` 0.11, `tar` 0.4, `flate2` 1.1, `serde_json` (already a
workspace dependency), `clap` (already a workspace dependency).

**Spec:** `docs/superpowers/specs/2026-09-01-wine-hosted-shim-design.md` §5.

**Deviation from the spec, approved by the user 2026-09-01:** §5 does not name an
implementation for fetching. Pure Rust was chosen over shelling out to
`curl`/`tar`/`sha512sum`, explicitly accepting ~15-25 new lockfile crates
including a TLS and cryptography stack the workspace does not have today, in
exchange for needing no host tools. Do not "simplify" this back to
`std::process::Command`.

## Global Constraints

- **No behaviour change on Windows**, and the whole crate must compile and test
  on **both** targets. Acquisition is deliberately portable: extracting a Linux
  tarball on Windows is useless but harmless, and the payoff is that URL
  building, digest parsing, version ordering and layout logic are covered by the
  Windows job.
- **`cargo clippy --all-targets -- -D warnings` must pass.** One clippy error
  masks every downstream crate.
- **No test may hit the network.** The real tarball is 533,700,853 bytes; a CI
  job that downloads it is unacceptable. Every test uses fixture JSON and
  locally-built tar.gz files. The one real download is a manual step in Task 4,
  run once, by hand.
- **Nothing may be written outside aether-vfs's own directory.** Never
  `~/.local/share/Steam`, never a system path. `umu`'s default download location
  is exactly what this exists to avoid.
- **A non-GE runtime is a hard error, never a warning.** `PROTONPATH` defaults to
  UMU-Proton, which is *stock* Valve Proton, so a silent fallback is the failure
  mode most likely to waste a day.
- **Every `unsafe` block** (there should be none in this crate) needs a
  `// SAFETY:` comment and `#[allow(unsafe_code)]`.
- All `cargo` commands run from `rust/`.
- **Linux verification** uses the Arch WSL box: repo cloned at
  `/root/aether-vfs` with this checkout as its `origin`. Pipe scripts via
  **stdin** to `bash -s` with `MSYS_NO_PATHCONV=1`; never `bash -c '<script>'`,
  which lets git-bash rewrite `/mnt/...` arguments and silently empty variables.
  Sync with `git fetch origin && git reset --hard origin/<branch>`, which sees
  only **committed** work.
- **Never read `$?` after a pipeline** — it yields the pipeline's last command's
  status. Redirect to a file, capture the code, then filter. This mistake has
  produced three wrong answers in this project already, including one that
  reported success for a process killed by SIGBUS.

---

### Task 1: The crate, its layout, and GE verification

**Files:**
- Create: `rust/crates/vfs-proton/Cargo.toml`
- Create: `rust/crates/vfs-proton/src/lib.rs`
- Create: `rust/crates/vfs-proton/src/layout.rs`
- Create: `rust/crates/vfs-proton/src/runtime.rs`
- Modify: `rust/Cargo.toml` (workspace members)

**Interfaces:**
- Produces, all consumed by Tasks 2-4:
  - `layout::Root` with `Root::from_env() -> io::Result<Root>`,
    `Root::at(base: PathBuf) -> Root`, `root.runtimes() -> PathBuf`,
    `root.runtime_dir(tag: &str) -> PathBuf`, `root.downloads() -> PathBuf`.
  - `runtime::verify_ge(dir: &Path) -> Result<String, VerifyError>` — reads the
    runtime's `version` file, confirms it names GE-Proton, returns the tag.
  - `runtime::installed(root: &Root) -> io::Result<Vec<String>>` — installed tags,
    sorted newest-first by `runtime::cmp_tags`.
  - `runtime::cmp_tags(a: &str, b: &str) -> Ordering` — orders `GE-ProtonN-M`
    numerically, not lexically.
  - `pub enum VerifyError { Missing, Unreadable(io::Error), NotGe(String) }`

- [ ] **Step 1: Create the manifest**

```toml
[package]
name = "vfs-proton"
version = "0.1.0"
edition = "2021"
publish = false
description = "GE-Proton acquisition and (later) launch: aether-vfs's self-contained runtime"

[lib]
name = "vfs_proton"
path = "src/lib.rs"

[[bin]]
name = "vfs-proton"
path = "src/bin/vfs-proton.rs"

[dependencies]
# Chosen over shelling out to curl/tar/sha512sum so a host needs no external
# tools; see the plan's deviation note. ureq's defaults already carry rustls +
# ring + webpki-roots, so TLS needs no extra wiring.
ureq = "3.4"
sha2 = "0.11"
tar = "0.4"
flate2 = "1.1"
serde_json = "1"
clap = { version = "4", features = ["derive"] }
```

Check the exact `clap` and `serde_json` versions already used elsewhere in the
workspace and match them rather than introducing a second major.

- [ ] **Step 2: Add to the workspace**

In `rust/Cargo.toml` add to `members`, after `"crates/vfs-unix",`:

```toml
  # GE-Proton acquisition. Portable on purpose: extracting a Linux tarball on
  # Windows is useless but harmless, and keeping it portable means the URL,
  # digest, version-ordering and layout logic are covered by the Windows job,
  # which is the thicker of the two.
  "crates/vfs-proton",
```

- [ ] **Step 3: Write the failing tests**

`rust/crates/vfs-proton/src/runtime.rs`, tests at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir()
            .join(format!("vfs-proton-rt-{}-{tag}", std::process::id()));
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

    #[test]
    fn tags_order_numerically_not_lexically() {
        // "GE-Proton11-6" < "GE-Proton9-1" as strings, which would make 9 newer
        // than 11 and pick the wrong default runtime.
        assert_eq!(cmp_tags("GE-Proton11-6", "GE-Proton9-1"), Ordering::Greater);
        assert_eq!(cmp_tags("GE-Proton11-10", "GE-Proton11-9"), Ordering::Greater);
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
            ("junk-dir", "1 proton-9.0-4\n"),   // not GE -> excluded
            ("half-extracted", ""),              // no version file -> excluded
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
}
```

`rust/crates/vfs-proton/src/layout.rs`, tests at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_places_everything_under_the_given_base() {
        let r = Root::at(std::path::PathBuf::from("/tmp/aebase"));
        assert_eq!(r.runtimes(), std::path::Path::new("/tmp/aebase/runtimes"));
        assert_eq!(
            r.runtime_dir("GE-Proton11-6"),
            std::path::Path::new("/tmp/aebase/runtimes/GE-Proton11-6")
        );
        assert_eq!(r.downloads(), std::path::Path::new("/tmp/aebase/downloads"));
    }

    #[test]
    fn a_tag_cannot_escape_the_runtimes_directory() {
        // Tags reach this from a CLI argument and from a GitHub release name, so
        // a traversal attempt must not resolve outside `runtimes()`.
        for evil in ["../../etc", "..", "a/../../b", "/absolute", "a/b"] {
            assert!(
                Root::at(std::path::PathBuf::from("/tmp/aebase"))
                    .try_runtime_dir(evil)
                    .is_err(),
                "tag {evil:?} must be refused"
            );
        }
        assert!(Root::at(std::path::PathBuf::from("/tmp/aebase"))
            .try_runtime_dir("GE-Proton11-6")
            .is_ok());
    }
}
```

- [ ] **Step 4: Run them to verify they fail**

Run: `cargo test -p vfs-proton --no-fail-fast`
Expected: compile failure — the modules and functions do not exist.

- [ ] **Step 5: Implement**

`layout.rs` holds `Root`. `Root::from_env` resolves the base directory as
`$AETHER_VFS_HOME` if set, else `$XDG_DATA_HOME/aether-vfs`, else
`$HOME/.local/share/aether-vfs`; on Windows fall back to `%LOCALAPPDATA%\aether-vfs`
so the crate is usable for tests there. `runtime_dir` is the infallible
convenience used by tests with known-good tags; `try_runtime_dir` validates and
is what every caller handling untrusted input must use — reject any tag that is
empty, absolute, or contains a path separator or `..`.

`runtime.rs` holds `verify_ge`, `installed`, `cmp_tags`, `VerifyError`. Derive
`Debug` on `VerifyError` (the tests format it). `verify_ge` reads `version`,
takes the whitespace-separated token that starts with `GE-Proton`, and returns
it; if a token exists but does not start with `GE-Proton`, return
`NotGe(<the file's trimmed contents>)`. `cmp_tags` parses the two integers out of
`GE-ProtonN-M` and compares `(N, M)` as numbers, ordering unparseable tags below
parseable ones so junk never wins a "newest" selection.

`lib.rs` declares the modules, re-exports the names above, and carries the
crate-level doc explaining that acquisition is portable and why.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p vfs-proton --no-fail-fast` — expect all 8 passing.
Then `cargo clippy -p vfs-proton --all-targets -- -D warnings` — expect clean.

- [ ] **Step 7: Commit**

```bash
git add rust/crates/vfs-proton rust/Cargo.toml rust/Cargo.lock
git commit -m "feat(proton): the runtime directory layout and a GE-only gate

Rejecting stock Proton is the point, not a nicety: PROTONPATH defaults to
UMU-Proton, which is Valve's stock build, so a silent fallback is the failure
mode most likely to cost a day. Tags order numerically because
\"GE-Proton11-6\" < \"GE-Proton9-1\" as strings, which would make 9 the newest."
```

---

### Task 2: Resolve a release from the GitHub API

**Files:**
- Create: `rust/crates/vfs-proton/src/release.rs`
- Create: `rust/crates/vfs-proton/tests/fixtures/releases.json`
- Modify: `rust/crates/vfs-proton/src/lib.rs`

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces:
  - `pub struct Release { pub tag: String, pub tarball_url: String, pub digest_url: String, pub size: u64 }`
  - `release::parse_releases(json: &str) -> Result<Vec<Release>, ResolveError>` —
    pure, no network, and the only thing tests exercise.
  - `release::pick(releases: &[Release], major: Option<u32>) -> Option<&Release>` —
    newest by `runtime::cmp_tags`, optionally constrained to a major series.
  - `release::fetch_releases(agent: &ureq::Agent) -> Result<Vec<Release>, ResolveError>` —
    the networked wrapper. **No test calls this.**
  - `pub enum ResolveError { Http(String), Json(String), NoAsset(String) }`

- [ ] **Step 1: Create the fixture**

`tests/fixtures/releases.json` — a trimmed but structurally faithful excerpt of
`https://api.github.com/repos/GloriousEggroll/proton-ge-custom/releases`. Include
**three** releases so ordering is observable: `GE-Proton11-6`, `GE-Proton11-5`,
and `GE-Proton9-1`. Each needs an `assets` array carrying both
`GE-ProtonX-Y-x86_64.tar.gz` (with a `size` and a `browser_download_url`) and
`GE-ProtonX-Y-x86_64.sha512sum`. Also include one release whose assets are
**aarch64 only**, to prove selection is architecture-aware and does not simply
take the first asset. Use the real byte size 533700853 for the 11-6 tarball.

- [ ] **Step 2: Write the failing tests**

In `release.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        std::fs::read_to_string(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/releases.json"),
        )
        .unwrap()
    }

    #[test]
    fn parses_tag_urls_and_size() {
        let rs = parse_releases(&fixture()).unwrap();
        let r = rs.iter().find(|r| r.tag == "GE-Proton11-6").unwrap();
        assert!(r.tarball_url.ends_with("GE-Proton11-6-x86_64.tar.gz"));
        assert!(r.digest_url.ends_with("GE-Proton11-6-x86_64.sha512sum"));
        assert_eq!(r.size, 533_700_853);
    }

    #[test]
    fn skips_releases_with_no_x86_64_asset() {
        // The aarch64-only release must not appear: taking "the first asset"
        // would install an ARM runtime on an x86_64 host.
        let rs = parse_releases(&fixture()).unwrap();
        assert!(
            rs.iter().all(|r| r.tarball_url.contains("x86_64")),
            "an aarch64-only release leaked through"
        );
    }

    #[test]
    fn pick_takes_the_newest_and_honours_a_major_series() {
        let rs = parse_releases(&fixture()).unwrap();
        assert_eq!(pick(&rs, None).unwrap().tag, "GE-Proton11-6");
        assert_eq!(pick(&rs, Some(11)).unwrap().tag, "GE-Proton11-6");
        assert_eq!(pick(&rs, Some(9)).unwrap().tag, "GE-Proton9-1");
        assert!(pick(&rs, Some(42)).is_none());
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(matches!(parse_releases("not json"), Err(ResolveError::Json(_))));
        assert!(matches!(parse_releases("{}"), Err(ResolveError::Json(_))));
    }
}
```

- [ ] **Step 3: Run to verify failure, then implement**

Parse with `serde_json::Value` rather than deriving structs — the API returns far
more than we need and a derived struct would break on unrelated schema changes.
For each release read `tag_name`, then scan `assets` for names ending
`-x86_64.tar.gz` and `-x86_64.sha512sum`; a release missing either is skipped
rather than erroring, because the upstream project has published incomplete
releases before. `fetch_releases` GETs
`https://api.github.com/repos/GloriousEggroll/proton-ge-custom/releases?per_page=30`
with a `User-Agent` header (GitHub rejects requests without one) and passes the
body to `parse_releases`.

- [ ] **Step 4: Tests and clippy, then commit**

```bash
git add rust/crates/vfs-proton
git commit -m "feat(proton): resolve a GE release, architecture-aware and newest-first

Selection skips releases lacking an x86_64 tarball rather than taking the first
asset, which would install an ARM runtime on an x86_64 host. Parses via
serde_json::Value, not a derived struct, so unrelated GitHub schema additions
cannot break it. No test touches the network."
```

---

### Task 3: Download, verify, extract — atomically

**Files:**
- Create: `rust/crates/vfs-proton/src/install.rs`
- Modify: `rust/crates/vfs-proton/src/lib.rs`

**Interfaces:**
- Consumes: `layout::Root`, `runtime::verify_ge`, `release::Release`.
- Produces:
  - `install::parse_sha512sum(body: &str) -> Result<String, InstallError>` — pure.
  - `install::verify_digest(path: &Path, expected_hex: &str) -> Result<(), InstallError>` — pure.
  - `install::extract_tar_gz(archive: &Path, into: &Path) -> Result<PathBuf, InstallError>` —
    returns the single top-level directory it unpacked.
  - `install::install_release(root: &Root, rel: &Release, agent: &ureq::Agent, force: bool) -> Result<Installed, InstallError>`
  - `pub struct Installed { pub tag: String, pub dir: PathBuf, pub fresh: bool }`
  - `pub enum InstallError { Io(io::Error), Http(String), Digest { expected: String, actual: String }, BadSha512Line(String), Archive(String), NotGe(String), Traversal(String) }`

- [ ] **Step 1: Write the failing tests**

```rust
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
            h.set_path("../escaped.txt").unwrap();
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
}
```

- [ ] **Step 2: Run to verify failure, then implement**

`install_release` sequence, and the ordering is the point:

1. If `root.try_runtime_dir(&rel.tag)` exists and `verify_ge` accepts it and
   `!force`, return `Installed { fresh: false }` — idempotent, no download.
2. GET `rel.digest_url`, `parse_sha512sum`.
3. Stream `rel.tarball_url` to `root.downloads()/<tag>.tar.gz.partial`. Streaming
   matters: 533 MB must not be buffered in memory.
4. `verify_digest`. **On mismatch, delete the partial file** and return
   `Digest { .. }`. A failed download must leave nothing reusable.
5. Extract into `root.runtimes()/.tmp-<tag>-<pid>/`, refusing traversal.
6. `verify_ge` on the extracted top-level directory. A tarball that is not GE is
   rejected *here too*, not only at the digest step.
7. `fs::rename` the extracted directory to `root.try_runtime_dir(&rel.tag)`.
   Rename last, so a half-extracted tree is never visible under the real name.
8. Remove the temp dir and the `.partial` file.

Use `ureq`'s reader for step 3 and `std::io::copy` into the file. Sha-512 with
`sha2::Sha512`, hashing in chunks from a `BufReader` rather than reading the file
into memory.

For traversal refusal in `extract_tar_gz`: do not rely on `tar`'s default
behaviour. For each entry, take its path and reject it if any component is
`ParentDir`, if it is absolute, or if it has a prefix/root — then join it onto
`into` yourself.

- [ ] **Step 3: Tests, clippy, commit**

Run `cargo test -p vfs-proton --no-fail-fast` (expect the earlier tests plus 5
here) and `cargo clippy -p vfs-proton --all-targets -- -D warnings`.

```bash
git add rust/crates/vfs-proton
git commit -m "feat(proton): download, verify and extract atomically

Rename is the last step, so a half-extracted tree is never visible under the
real runtime name and a failed install cannot look installed. A digest mismatch
deletes the partial file rather than leaving something reusable. Extraction
refuses traversing members explicitly instead of trusting the tar crate's
defaults, and a 533 MB body is streamed, never buffered.

verify_ge runs on the extracted tree as well as at resolution time: being the
right bytes and being GE-Proton are separate claims."
```

---

### Task 4: The command, CI, and one real install

**Files:**
- Create: `rust/crates/vfs-proton/src/bin/vfs-proton.rs`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes everything above. Produces the `vfs-proton` binary.

- [ ] **Step 1: Implement the CLI**

`clap` derive, three subcommands:

- `install [--version <TAG>] [--major <N>] [--dir <PATH>] [--force]` — with no
  `--version`, resolve the newest release in major series `--major`, **defaulting
  to 11**, because the user's stated preference is GE-Proton 11.x. Print the
  resolved tag before downloading, then the install directory, and say plainly
  whether it downloaded or was already present.
- `list [--dir <PATH>]` — installed runtimes, newest first, one per line, each
  with its verified tag and path. Empty output and exit 0 when none.
- `path [--version <TAG>] [--dir <PATH>]` — print the directory for the newest
  installed runtime (or the named one) and exit 0; exit non-zero with a message
  on stderr if it is absent. This is the form a shell script or a host uses to
  set `PROTONPATH`, so stdout must carry the path **and nothing else**.

Report progress during the download — a 533 MB fetch with no output looks hung.
Print percentage against `Release::size` at most once a second, to stderr, so
`path`-style stdout capture stays clean.

- [ ] **Step 2: Cover it in CI on both jobs**

Add `-p vfs-proton` to the Windows job's test command and to the
`rust-linux-portable` job's "Portable Rust crates" command. Both already carry
`--no-fail-fast`. Add a comment on the Linux one noting the crate is portable on
purpose and that **no test in it touches the network**.

- [ ] **Step 3: Verify — including one real, manual install**

Windows: `cargo test -p vfs-proton --no-fail-fast`,
`cargo clippy --all-targets -- -D warnings`.

Linux, in Arch, after committing:

```bash
cargo run -p vfs-proton --bin vfs-proton -- list --dir /root/aether-test
cargo run -p vfs-proton --bin vfs-proton -- install --dir /root/aether-test
cargo run -p vfs-proton --bin vfs-proton -- list --dir /root/aether-test
cargo run -p vfs-proton --bin vfs-proton -- path --dir /root/aether-test
```

This downloads ~533 MB once. Expected: `list` empty at first; `install` resolves
`GE-Proton11-6` (or newer), verifies the digest, and reports the install
directory; the second `list` shows it; `path` prints the directory alone.

Then prove three properties that separate this from a naive downloader:

1. **Idempotence** — run `install` again; it must report already-present and
   download nothing (visibly faster, no progress output).
2. **Digest enforcement** — corrupt a byte in the installed tarball copy or
   point `--version` at a tag whose digest you have altered locally, and confirm
   the failure is a `Digest` mismatch naming both hashes. If that is impractical
   without network trickery, instead verify by unit test that
   `verify_digest` rejects, and **say in your report that the end-to-end
   corruption path was not exercised** rather than implying it was.
3. **The runtime is genuinely GE** — `cat <path>/version` must name GE-Proton,
   and the installed tree must contain `files/bin/wine`.

- [ ] **Step 4: Commit**

```bash
git add rust/crates/vfs-proton .github/workflows/ci.yml
git commit -m "feat(proton): the vfs-proton command, and CI for it on both targets

install/list/path, with install defaulting to the newest GE-Proton 11.x because
that is the stated preference. `path` prints the directory on stdout and nothing
else, so a host can set PROTONPATH from it; progress goes to stderr for the same
reason. No test touches the network: the 533 MB download is a manual step."
```

---

## Self-Review

**Spec coverage.** §5's acquisition, verified digest, aether-owned storage, and
the GE-not-stock gate are Tasks 1-4. §5's prefix construction, drive-letter
rerouting and `dosdevices/z:` removal are **not** in this plan — they are launch,
and belong to the next increment. §6's identity gap is untouched.

**Type consistency.** `Root` is constructed by `Root::at` in tests and
`Root::from_env` in the binary; `try_runtime_dir` is the validating form used by
anything handling a tag from the CLI or the API, and `runtime_dir` the infallible
form used only with literals. `Release` carries `size` because Task 4's progress
output needs a denominator. `cmp_tags` is defined in Task 1 and used by Task 2's
`pick`.

**Known soft spots, stated not hidden.** Task 2's fixture is described rather
than written out, because a faithful GitHub releases excerpt is long and
mechanical; its required *contents* are specified precisely (three releases, one
aarch64-only, the real 533700853 size). Task 4 Step 3's corruption check may not
be reachable without network trickery, and the step says explicitly to report
that rather than imply coverage. `install_release`'s eight steps are prose, not
code — the ordering is the substance and is enumerated exactly, but an
implementer must write the body.
