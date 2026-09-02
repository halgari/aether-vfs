# `Session::launch` on Linux — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `vfs_embed::Session::serve()` then `launch()` starts a Windows
executable under GE-Proton on Linux, with the real shim injected and its ring
served by the native Linux `Director`.

**Architecture:** Every mechanism this needs is already proven; what is missing
is the wiring. The injector's own doc already describes the role Linux must
play — *"the JVM sets the ring env, spawns this bin; this bin injects the shim
into the target, which inherits the env and connects its FuseClient back to the
ring"* — so `vfs-proton` becomes another such host: build the config, export the
env, run `wine vfs-injector.exe`.

**Spec:** `docs/superpowers/specs/2026-09-01-wine-hosted-shim-design.md` §5.

## What is already proven (do not re-litigate)

- `retour` detours patch Wine's PE ntdll; `NtCreateFile`/`NtOpenFile`/`NtReadFile`
  redirection works under GE-Proton11-6.
- `CreateProcessW` + remote DLL injection works under Wine — `vfs-injector.exe`
  loaded `vfs_shim_dll.dll` into a child (differing thread ids in a `+loaddll`
  trace).
- The **real shim** in a Wine process is served by a **native Linux Director**
  over a file-backed ring, inline and bulk (`vfs-serve-fb` + `shim-ring-client`).
- `IpcServe::start_file_backed` exists on unix and is what `Session` must use.
- `vfs-proton` acquires and verifies GE-Proton (`install` / `list` / `path`).

## Global Constraints

- **Windows behaviour must not change.** The named-section `serve`/`launch` path
  is what every existing Windows test uses. This adds a unix path beside it.
- **No new env switch unless registered** in `vfs_env::ALL`; a workspace lint
  enforces it and has caught this project twice.
- **The protocol descriptor must not drift.** `bin/regen-protocol` then
  `git diff --exit-code resources/` stays clean. Task 1 moves a wire encoder, so
  this is a real risk there, not boilerplate.
- `cargo clippy --all-targets -- -D warnings` must pass.
- **Verify at workspace scope** — `cargo test --no-fail-fast` with
  `TMP=C:\vfstmp` and `TEMP=C:\vfstmp`.
- **Never read `$?` after a pipeline**, and never judge a result through `tail`.
  Write output to a file and read it. This has produced three wrong answers here,
  one reporting success for a SIGBUS'd process.
- WSL: pipe scripts via **stdin** to `bash -s` with `MSYS_NO_PATHCONV=1`; the
  Arch clone at `/root/aether-vfs` sees only **committed** work.
- Bound every Wine run with `timeout`, and kill what you start: a wedged fixture
  keeps `vfs_shim_dll.dll` mapped and then no build can replace it.

---

### Task 1: A portable shim-config encoder

**Files:**
- Create: `rust/crates/vfs-protocol/src/shimcfg.rs`
- Modify: `rust/crates/vfs-protocol/src/lib.rs`
- Modify: `rust/crates/vfs-shim/src/bootstrap.rs` (re-export, keep the decoder)

**Interfaces:**
- Produces `vfs_protocol::shimcfg::{StaticImport, encode_config, encode_config_full, encode_config_with_overlay}`
  with signatures identical to today's `vfs_shim` ones:
  `encode_config_full(root: &str, overlay: &str, static_imports: &[StaticImport], snapshot: &[u8]) -> Vec<u8>`.
- `vfs_shim` re-exports all four names so every existing caller compiles
  unchanged (`vfs-embed/src/session.rs`, `vfs-inject`'s two tests,
  `vfs-shim/tests/exit_stall_repro.rs`).

**Why:** Linux must build a shim config, and today the encoder sits in
`vfs-shim`, which is Windows-only. The function itself is pure byte assembly —
only its enclosing module has Windows dependencies (`crate::engine`,
`crate::hook`, `crate::payload_abi`).

- [ ] **Step 1: Move the encoders, keep the decoder**

Move `encode_config`, `encode_config_full`, `encode_config_with_overlay` and the
`StaticImport` struct into the new module **verbatim**, including their doc
comments. Leave `decode_config_full` in `vfs-shim`: it is only used by the shim
itself, and moving it would widen this task for nothing.

Note `StaticImport` is currently declared **twice** — `vfs-shim/src/bootstrap.rs`
and `vfs-inject/src/static_imports.rs`, both `{ dll_name: String, backing_path:
String }`. Do **not** try to unify them here; that is a separate change. Move the
`vfs-shim` one and leave `vfs-inject`'s alone, and say so in your report.

- [ ] **Step 2: Re-export from `vfs-shim`**

`vfs-shim/src/lib.rs` already exports these names (line ~24). Keep that export
list identical by re-exporting from `vfs_protocol` instead of `bootstrap`, so no
caller changes.

- [ ] **Step 3: Prove the bytes did not change**

This is a wire format with a pinned descriptor, so identity matters more than
compilation. Add to the new module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The wire bytes are a pinned format, so this asserts the exact encoding
    /// rather than a round trip: a round trip would pass even if both sides
    /// moved together, which is precisely the regression that would break an
    /// already-shipped shim.
    #[test]
    fn encode_config_full_layout_is_unchanged() {
        let out = encode_config_full("R", "O", &[], &[7, 8]);
        // len("R")=1, "R", len("O")=1, "O", then the static-import count, then
        // the snapshot. Read the function to confirm field order before
        // trusting this comment.
        assert_eq!(&out[0..4], &1u32.to_le_bytes());
        assert_eq!(out[4], b'R');
        assert_eq!(&out[5..9], &1u32.to_le_bytes());
        assert_eq!(out[9], b'O');
        assert!(out.ends_with(&[7, 8]), "snapshot must be last: {out:?}");
    }

    #[test]
    fn with_overlay_matches_full_with_no_static_imports() {
        assert_eq!(
            encode_config_with_overlay("R", "O", &[1, 2, 3]),
            encode_config_full("R", "O", &[], &[1, 2, 3])
        );
    }
}
```

If the field order in the assertion does not match the function, **fix the
assertion, not the function** — and note in your report what the real order is.

- [ ] **Step 4: Verify and commit**

`cargo test -p vfs-protocol -p vfs-shim -p vfs-inject -p vfs-embed --no-fail-fast`,
then the descriptor gate: `bin/regen-protocol` followed by
`git diff --exit-code resources/`. Both must be clean.

```bash
git add rust/crates/vfs-protocol rust/crates/vfs-shim
git commit -m "refactor(protocol): the shim-config encoder becomes portable"
```

---

### Task 2: Session prefixes and drive rerouting

**Files:**
- Create: `rust/crates/vfs-proton/src/prefix.rs`
- Modify: `rust/crates/vfs-proton/src/lib.rs`

**Interfaces:**
- Consumes `layout::Root`, `runtime::verify_ge`.
- Produces:
  - `prefix::Prefix { pub dir: PathBuf }`
  - `prefix::ensure(root: &Root, runtime: &Path, session: &str) -> Result<Prefix, PrefixError>`
    — creates `root.sessions()/<session>/prefix` and runs `wineboot -u` if it
    looks uninitialised; idempotent.
  - `prefix.drive_c(&self) -> PathBuf`
  - `prefix.map_drive(&self, letter: char, target: &Path) -> Result<(), PrefixError>`
    — points `dosdevices/<letter>:` at `target`.
  - `prefix.unmap_drive(&self, letter: char) -> Result<(), PrefixError>`
  - `prefix.windows_path(&self, host: &Path) -> Option<String>` — host path →
    the `C:\…` form Wine sees, for paths under `drive_c`.
  - `pub enum PrefixError { Io(io::Error), Wineboot(String), NotGe(String), Missing32Bit }`

**Measured facts to build on (do not rediscover):**
- A fresh prefix has `dosdevices/c: -> ../drive_c` and `dosdevices/z: -> /`, and
  is about **627 MB**.
- `wineboot` fails with `/lib/ld-linux.so.2: could not open` unless a 32-bit
  runtime is present (`lib32-glibc`, `lib32-gcc-libs` on Arch). `WINEARCH=win64`
  does **not** avoid this — the `wine` launcher probes the 32-bit loader
  regardless. Detect this and return `Missing32Bit` with a message naming the
  packages, rather than surfacing Wine's error.
- FreeType warnings are cosmetic for console targets; do not treat them as
  failure.

- [ ] **Step 1: Write the failing tests**

These must not need Wine, so they test the parts that are pure:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("vfs-prefix-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn windows_path_maps_only_paths_under_drive_c() {
        let p = Prefix { dir: scratch("wp") };
        std::fs::create_dir_all(p.drive_c().join("Games")).unwrap();
        let inside = p.drive_c().join("Games").join("g.exe");
        assert_eq!(
            p.windows_path(&inside).as_deref(),
            Some(r"C:\Games\g.exe"),
            "a path under drive_c must render as a C: path with backslashes"
        );
        assert_eq!(
            p.windows_path(std::path::Path::new("/etc/passwd")),
            None,
            "a path outside the prefix has no C: form and must not be invented"
        );
    }

    #[cfg(unix)]
    #[test]
    fn map_drive_points_dosdevices_at_the_target_and_unmap_removes_it() {
        let p = Prefix { dir: scratch("drv") };
        std::fs::create_dir_all(p.dir.join("dosdevices")).unwrap();
        let target = scratch("drv-target");
        p.map_drive('d', &target).unwrap();
        let link = p.dir.join("dosdevices").join("d:");
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        // Remapping must replace, not fail: a session reuses a prefix.
        let target2 = scratch("drv-target2");
        p.map_drive('d', &target2).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), target2);
        p.unmap_drive('d').unwrap();
        assert!(!link.exists(), "unmap must remove the link");
    }

    #[cfg(unix)]
    #[test]
    fn unmapping_z_is_how_containment_is_achieved() {
        // `dosdevices/z: -> /` maps the whole host filesystem into the game's
        // namespace. Removing it gives containment Windows does not have, so
        // this is a feature and needs to keep working.
        let p = Prefix { dir: scratch("z") };
        let dd = p.dir.join("dosdevices");
        std::fs::create_dir_all(&dd).unwrap();
        std::os::unix::fs::symlink("/", dd.join("z:")).unwrap();
        p.unmap_drive('z').unwrap();
        assert!(!dd.join("z:").exists());
    }
}
```

- [ ] **Step 2: Implement**

`ensure` treats a prefix as initialised when `drive_c/windows/system32` exists;
otherwise it runs `<runtime>/files/bin/wine wineboot -u` with `WINEPREFIX` set,
`WINEDLLOVERRIDES="mscoree=d;mshtml=d"` and `WINEDEBUG=-all`. Before running, it
calls `verify_ge(runtime)` and refuses a non-GE runtime — `PROTONPATH` defaults
to stock Proton and a silent downgrade is the failure this design exists to
prevent. If `wineboot` fails, check its output for `ld-linux.so.2` and return
`Missing32Bit` rather than the raw error.

`windows_path` strips the `drive_c` prefix and joins with `\` after a `C:`; it
returns `None` for anything not under `drive_c`.

- [ ] **Step 3: Verify and commit**

Tests on Windows (the pure ones) and on Linux (all of them). Then commit.

```bash
git add rust/crates/vfs-proton
git commit -m "feat(proton): session prefixes, drive rerouting, and dropping Z:"
```

---

### Task 3: Launch under Wine

**Files:**
- Create: `rust/crates/vfs-proton/src/launch.rs`
- Modify: `rust/crates/vfs-proton/src/lib.rs`

**Interfaces:**
- Consumes `prefix::Prefix`, `runtime::verify_ge`.
- Produces:
  - `pub struct WineLaunch { pub runtime: PathBuf, pub prefix: PathBuf, pub injector: PathBuf, pub shim_dll: PathBuf, pub payload_dll: PathBuf, pub target: String, pub config_file: PathBuf, pub ready_file: PathBuf, pub ring_path: PathBuf, pub ring_bytes: usize, pub arena_offset: usize, pub arena_len: usize, pub payload_cap: u32, pub virtual_dir: String, pub args: Vec<String> }`
  - `launch::run(l: &WineLaunch) -> Result<i32, LaunchError>` — spawns
    `wine <injector> <target> <shim> <payload> <cfg> <ready> [-- args…]`, waits,
    returns the child's exit code.
  - `pub enum LaunchError { Io(io::Error), NotGe(String), Spawn(String), NonZeroWine(i32) }`

**The env is the substance of this task.** Read `vfs-shim/src/fuse_client.rs`
for the exact set it requires to consider itself configured, and set precisely
those. Known-necessary, from the working cross-boundary run:

| variable | value |
|---|---|
| `WINEPREFIX` | the prefix dir |
| `PROTONPATH` | the runtime dir, **absolute** |
| `WINEDLLOVERRIDES` | `mscoree=d;mshtml=d` |
| `VFS_RING_PATH` | the ring file **as Wine sees it** (`C:\…`) |
| `VFS_RING_BYTES` | the Director's **true** map size |
| `VFS_ARENA_LEN`, `VFS_ARENA_OFFSET` | from the `IpcServe` |
| `VFS_RING_PAYLOAD_CAP` | from the `IpcServe` |
| `VFS_VIRTUAL_DIR` | the managed root as Wine sees it |

**`VFS_RING_BYTES` must be the Director's real map size, not a default.**
Measured: with the 2 MiB default against a ~34 MiB ring, a 256 KiB read *passes*
(its arena bank lands inside the mapped view) and only a 4 MiB read fails, with
the server logging every read answered. Under-mapping is silent at attach and
fatal under load.

**Verify GE before launching** and return `NotGe` otherwise.

- [ ] **Step 1: Write the failing test**

A launch needs Wine, so unit-test the command construction instead — which is
where the mistakes live:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> WineLaunch { /* fill every field with recognisable values */ }

    #[test]
    fn argv_is_the_injector_contract_in_order() {
        // vfs-injector <target_exe> <shim_dll> <payload_dll> <config> <ready>
        // [-- args]. Order is positional, so a swap is silent and this is the
        // only thing that catches it.
        let (prog, argv) = command_line(&sample());
        assert!(prog.ends_with("wine"), "{prog}");
        assert_eq!(argv[0], sample().injector.to_string_lossy());
        assert_eq!(argv[1], sample().target);
        assert_eq!(argv[2], sample().shim_dll.to_string_lossy());
        assert_eq!(argv[3], sample().payload_dll.to_string_lossy());
        assert_eq!(argv[4], sample().config_file.to_string_lossy());
        assert_eq!(argv[5], sample().ready_file.to_string_lossy());
    }

    #[test]
    fn env_carries_the_real_ring_size_not_a_default() {
        let mut l = sample();
        l.ring_bytes = 33_751_040;
        let env = launch_env(&l);
        assert_eq!(env.get("VFS_RING_BYTES").map(String::as_str), Some("33751040"));
        assert!(env.contains_key("VFS_ARENA_LEN"));
        assert!(env.contains_key("VFS_ARENA_OFFSET"));
        assert!(env.contains_key("VFS_RING_PAYLOAD_CAP"));
    }

    #[test]
    fn protonpath_is_absolute_because_a_relative_one_silently_means_stock() {
        let env = launch_env(&sample());
        let p = env.get("PROTONPATH").expect("PROTONPATH");
        assert!(std::path::Path::new(p).is_absolute(), "{p}");
    }
}
```

Split `command_line` and `launch_env` out as pure functions so they are testable
without spawning anything. That split is the point of the task's design.

- [ ] **Step 2: Implement, verify, commit**

```bash
git add rust/crates/vfs-proton
git commit -m "feat(proton): launch a Windows target under GE-Proton with the shim injected"
```

---

### Task 4: `Session::serve` and `Session::launch` on Linux

**Files:**
- Modify: `rust/crates/vfs-embed/src/session.rs`
- Modify: `rust/crates/vfs-embed/Cargo.toml`

**Interfaces:**
- Consumes Tasks 1–3 plus `IpcServe::start_file_backed`.
- Produces: `Session::serve()` and `Session::launch()` with **unchanged
  signatures**, now working on unix. `LaunchOpts` gains nothing.

**Context:** `session.rs` currently gates `use vfs_director::ipc::IpcServe` and
the `ipc: Option<IpcServe>` field to Windows, and its own comment says why — the
type is portable now, but "the only constructor `Session` uses is the
Windows-only named-section one". Task 4 removes that gate and gives unix the
file-backed constructor.

`vfs-proton` becomes a `[target.'cfg(unix)'.dependencies]` entry of `vfs-embed`.

- [ ] **Step 1: Make the field and import portable**

Drop both `#[cfg(windows)]`s, and delete the now-false comment about there being
no `Option<IpcServe>` on unix rather than leaving it to mislead.

- [ ] **Step 2: `serve()` on unix**

Same shape as the Windows body, but the ring is a **file** under
`state_dir/ring.bin` via `IpcServe::start_file_backed`, and there is no section
name or event pair. Keep the Windows body byte-for-byte as it is.

- [ ] **Step 3: `launch()` on unix**

Resolve the runtime with `vfs_proton::runtime`/`layout` (honouring `VFS_HOME`),
`prefix::ensure`, then build a `WineLaunch` from the live `IpcServe`'s geometry
and `vfs_proton::launch::run`. The config comes from Task 1's portable encoder.

The Windows artifacts (`vfs-injector.exe`, `vfs_shim_dll.dll`, `vfs_payload.dll`)
cannot be built on Linux. Take them from `LaunchOpts` if it already carries
`shim_dll`/`payload_dll` fields (read it), else from a documented location beside
the image, and fail with a message naming what is missing rather than a path
error.

- [ ] **Step 4: Verify and commit**

Windows: workspace scope, and `cargo test -p vfs-embed` must have the **same
count** as before. Linux: `cargo check -p vfs-embed --tests --target
x86_64-unknown-linux-gnu` clean including warnings, plus
`cargo tree -p vfs-embed --target x86_64-unknown-linux-gnu | grep -iE "windows|retour|udis86"`
printing nothing.

---

### Task 5: A Windows fixture launched by `Session` on Linux

**Files:**
- Create: `rust/crates/vfs-embed/tests/proton_launch.rs` (`#![cfg(unix)]`)

**This is the increment's definition of done**, and the first time the product
API — not a harness — starts a Windows process under Proton and serves it.

- [ ] **Step 1: The test**

Build a `Session` over one disk-backed entry, `serve()`, then `launch()` a
Windows fixture that reads a file existing only in the provider. Assert the
child's exit code and that the read succeeded.

Mark it `#[ignore]` with a reason naming what it needs (a GE-Proton runtime, a
prefix, and Windows-built artifacts), so CI stays green while the test remains
runnable by name. Say in your report exactly how to run it.

- [ ] **Step 2: Run it for real in the Arch box**

GE-Proton is installed at `/root/aether/runtimes/GE-Proton11-6-x86_64`; a prefix
exists at `/root/aether/probe-prefix`. Copy the Windows artifacts in from
`/mnt/c/oss/aether-vfs/rust/target/debug`. Report the literal output of both the
test and the Director.

If it does not work, the diagnosis is worth more than a fix: report which side
logged what, and whether the ring geometry matched.

---

## Self-Review

**Spec coverage.** §5's prefix construction, drive rerouting, `Z:` removal, GE
enforcement and launch are Tasks 2–3; §3's "an embedder gets the right delivery
for their platform" is Task 4. Nothing here touches the identity gap (§6), which
is closed, or umu (deferred with reasons).

**Type consistency.** `WineLaunch` carries `ring_bytes`/`arena_offset`/
`arena_len`/`payload_cap` because Task 4 must pass the Director's *real*
geometry, which is the silent-failure risk called out in Task 3.
`prefix::Prefix` is a plain `{ dir }` so tests can build one without Wine.

**Known soft spots.** Task 3's `sample()` is described rather than written, and
Task 5's test body likewise — both need field names read from the code rather
than guessed. Task 4's artifact-location question is genuinely open: I do not
know whether `LaunchOpts` already carries `shim_dll`/`payload_dll`, so the task
says to read it and decide, and to report the choice.
