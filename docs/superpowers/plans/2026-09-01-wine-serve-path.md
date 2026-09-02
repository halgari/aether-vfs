# The Wine serve path — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Windows fixture running under GE-Proton, with the real shim injected,
served by a **native Linux Director** over the file-backed ring.

**Architecture:** Both ends already exist; they just cannot find each other. The
Director's `worker_loop` is already generic over `N: Notifier` and touches only
portable pieces — its sole Windows dependency is the concrete
`vfs_win::{SharedMapping, EventNotifier}` pair. `vfs_unix::FileMapping` exposes
the same `seg()`/`len()`/`as_mut_ptr()` accessors by design, so the serve loop
becomes portable behind a type alias, with a second constructor that takes a ring
**path** instead of a section **name**. The shim selects file-backed mode from a
new `VFS_RING_PATH` env switch.

**Tech Stack:** Rust. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-01-wine-hosted-shim-design.md` §3, §5.

**Scope note.** This is the *data path*, not the product API. Prefix
construction, `dosdevices` rerouting and `Session::launch` on Linux are the
**next** increment; here the fixture is launched by the test harness. That split
is deliberate: the data path is the part with unknowns, and launching is
plumbing.

## Global Constraints

- **The Windows path must keep working unchanged.** Named-section +
  `EventNotifier` is what every existing Windows test and the shipped shim use.
  This increment *adds* a mode; it must not alter the default one.
- **`VFS_RING_PATH` must be registered in `vfs_env::ALL`.** A workspace lint
  enforces it and already caught this project once. Names must be `VFS_`-prefixed.
- `cargo clippy --all-targets -- -D warnings` must pass.
- Every `unsafe` block keeps `// SAFETY:` and `#[allow(unsafe_code)]`.
- **Verify at workspace scope** — `cargo test --no-fail-fast` for the whole
  workspace, with `TMP=C:\vfstmp` and `TEMP=C:\vfstmp`.
- **Never read `$?` after a pipeline**, and never judge a result through `tail`.
  Write output to a file and read it. Three wrong answers in this project came
  from the former, one reporting success for a SIGBUS'd process.
- WSL: pipe scripts via **stdin** to `bash -s` with `MSYS_NO_PATHCONV=1`; the
  Arch clone at `/root/aether-vfs` sees only **committed** work.
- Bound every Wine and server run with `timeout`. In file-backed mode both ends
  spin, so a geometry mismatch spins forever instead of failing.

## Why `notify_server` becomes a no-op, and what it costs

`vfs-shim`'s client notifier is `WakeServerSpinClient`, whose `notify_server`
calls `SetEvent(server_ev)`. Its doc records why: a plain `SpinNotifier`'s no-op
left a sleeping Director unaware until the 15.6 ms timer tick, and measured
2026-08-12, 16 of 231 `NtQueryFullAttributesFile` calls stalled that way and owned
~93% of that hook's total time.

**Under Wine that `SetEvent` cannot wake a native Linux process** — the event is a
Wine object. So in file-backed mode `notify_server` has nothing to signal and
becomes a no-op, and the Linux Director must **spin** rather than sleep. That
avoids the stall (a spinning server sees the request immediately) at the cost of
burning CPU. Accepted for this increment; a shared-memory futex is the eventual
answer and needs its own design, because Windows code cannot call `futex` without
breaking the one-codebase property.

Do not "fix" this by making the Linux Director sleep. It would reintroduce
exactly the stall above with nothing able to wake it.

---

### Task 1: Register `VFS_RING_PATH`

**Files:** Modify `rust/crates/vfs-env/src/lib.rs`

**Interfaces:** Produces `vfs_env::RING_PATH` (`"VFS_RING_PATH"`), consumed by
Tasks 2 and 3.

- [ ] **Step 1: Add the constant and the registry entry**

Beside `RING_SECTION`, add a `pub const RING_PATH: &str = "VFS_RING_PATH";` with
a doc comment saying: the ring's backing **file**, used instead of
`RING_SECTION` when the client and server are on opposite sides of a Wine
boundary, because a page-file-backed named section has no identity a native Linux
process can open. Add the matching `Var { name: RING_PATH, kind: Kind::Handshake,
default: "none (named section via VFS_RING_SECTION)" }` to `ALL`.

- [ ] **Step 2: Verify and commit**

Run `cargo test -p vfs-env --no-fail-fast`. All three registry lints must pass:
`every_name_is_unique_and_prefixed`, `describe_lists_every_switch`,
`no_crate_reads_a_switch_that_is_not_registered`.

```bash
git add rust/crates/vfs-env/src/lib.rs
git commit -m "feat(env): register VFS_RING_PATH, the file-backed ring's location"
```

---

### Task 2: A Linux serve path in the Director

**Files:**
- Modify: `rust/crates/vfs-director/src/ipc.rs`
- Modify: `rust/crates/vfs-director/src/lib.rs` (the `cfg(windows)` gate on `ipc`)
- Modify: `rust/crates/vfs-director/Cargo.toml`
- Test: `rust/crates/vfs-director/tests/serve_file_backed.rs` (new)

**Interfaces:**
- Consumes: `vfs_unix::FileMapping::create(&Path, usize)`, `vfs_env::RING_PATH`.
- Produces: `IpcServe::start_file_backed(kernel, ring_path: &Path, payload_cap: u32) -> Result<IpcServe, String>`,
  available on `unix`. Task 4 consumes it. Also produces
  `IpcServe::ring_path(&self) -> Option<&Path>` so a test can assert what it made.

- [ ] **Step 1: Make the serve machinery portable**

`ipc.rs` is currently `#[cfg(windows)]` at the module declaration in `lib.rs`.
Ungate it and instead gate *inside* the file, exactly as the earlier increment
did for `ring_dispatch`. Read how `lib.rs:36` gates `ipc` today and follow the
existing style.

Introduce the mapping alias:

```rust
/// The ring's shared-memory backing, chosen by target.
///
/// Both types expose `seg()`, `len()` and `as_mut_ptr()` with identical
/// meaning — deliberately, so everything above this line is written once.
/// `SharedMapping` is a named page-file-backed section; `FileMapping` is an
/// `mmap` over a real file, which is what lets a shim inside Wine and a native
/// Linux Director share one ring.
#[cfg(windows)]
type RingMapping = vfs_win::SharedMapping;
#[cfg(unix)]
type RingMapping = vfs_unix::FileMapping;
```

`Inner` holds `mapping: RingMapping`. Only its `seg()` is used by `worker_loop`,
so nothing else changes. The `_events: EventNotifier` field and the named-section
constructor stay `#[cfg(windows)]`; put `Inner`'s Windows-only fields behind cfg
rather than duplicating the struct.

Add to `Cargo.toml`:

```toml
[target.'cfg(unix)'.dependencies]
vfs-unix = { path = "../vfs-unix" }
```

- [ ] **Step 2: Write the failing test**

`tests/serve_file_backed.rs`:

```rust
//! The Director serving over a file-backed ring, with a same-process client.
//!
//! This is the Windows-free half of the Wine path: the mapping is a real file
//! and the notifier is a spin, so nothing here needs an OS event object. Task 4
//! puts the client inside Wine; this pins the server side first.
#![cfg(unix)]

use std::sync::Arc;

use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
use vfs_director::{Director, IpcServe};

#[test]
fn a_file_backed_serve_answers_a_getattr_and_a_read() {
    let dir = std::env::temp_dir().join(format!("vfs-serve-fb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let backing = dir.join("hello.txt");
    std::fs::write(&backing, b"served-over-a-file-backed-ring\n").unwrap();
    let ring = dir.join("ring.bin");

    // A Director over one disk-backed entry. Read the existing
    // `ring_dispatch.rs` tests for the shortest way to build this; mirror it
    // rather than inventing a second idiom.
    let kernel = Arc::new(Director::new());
    // ... mount a provider serving "data/hello.txt" from `backing` ...

    let serve = IpcServe::start_file_backed(kernel, &ring, 4096)
        .expect("file-backed serve must start");
    assert_eq!(serve.ring_path(), Some(ring.as_path()));
    assert!(ring.exists(), "the ring file must exist once serving");
    assert!(
        std::fs::metadata(&ring).unwrap().len() >= 2 * 1024 * 1024,
        "the mapping must be fully sized, or a client mmap faults on touch"
    );

    // Drive it with a client over the SAME file, opened independently — that is
    // the property Task 4 depends on.
    // ... open vfs_unix::FileMapping::open(&ring, ..), ring::open, RingClient
    //     with SpinNotifier, send OP_GETATTR then OP_READ, assert ST_OK and
    //     that the bytes match ...

    drop(serve);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn starting_twice_on_one_path_does_not_truncate_the_first_ring() {
    // `FileMapping::create` is grow-only precisely so this cannot SIGBUS the
    // first server; assert the file did not shrink.
    let dir = std::env::temp_dir().join(format!("vfs-serve-fb2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ring = dir.join("ring.bin");
    let a = IpcServe::start_file_backed(Arc::new(Director::new()), &ring, 4096).unwrap();
    let len_a = std::fs::metadata(&ring).unwrap().len();
    let b = IpcServe::start_file_backed(Arc::new(Director::new()), &ring, 4096).unwrap();
    assert_eq!(std::fs::metadata(&ring).unwrap().len(), len_a);
    drop(b);
    drop(a);
    let _ = std::fs::remove_dir_all(&dir);
}
```

Fill the elided sections by reading `crates/vfs-director/src/ring_dispatch.rs`'s
tests and `crates/vfs-server/tests/fuse_e2e.rs`, which already build a
`RingClient` with a `SpinNotifier` and drive `OP_GETATTR`/`OP_READ`. Do not
invent a new idiom.

- [ ] **Step 3: Implement `start_file_backed`**

`#[cfg(unix)]`. Same geometry arithmetic as `start` — copy it rather than
diverging, since the client computes the same numbers. Differences:

- `RingMapping::create(ring_path, map_size)` instead of a named section;
- no `EventNotifier`: every worker runs `worker_loop(&inner, SpinNotifier)`;
- store the ring path so `ring_path()` can report it.

- [ ] **Step 4: Verify**

Linux, in Arch: `cargo test -p vfs-director --no-fail-fast`.
Windows, workspace scope: `cargo test --no-fail-fast` plus
`cargo clippy --all-targets -- -D warnings`. The Windows serve path must be
untouched — confirm `vfs-directord`'s e2e tests still pass. (Note: that binary
has a known intermittent hang, `escape_matrix_positive_and_negative_canary` and
the `scenario_toml_*` tests. If you hit it, say so and distinguish it from a
regression you caused.)

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-director
git commit -m "feat(director): serve over a file-backed ring on unix"
```

---

### Task 3: File-backed mode in the shim

**Files:**
- Modify: `rust/crates/vfs-shim/src/fuse_client.rs`
- Test: extend an existing `vfs-shim` test or add one; see Step 3.

**Interfaces:**
- Consumes: `vfs_win::SharedMapping::open_file_backed(&Path, usize)`,
  `vfs_env::RING_PATH`.
- Produces: no new public API — `FuseClient::from_env` (or whatever
  `fuse_client.rs` calls its constructor; read it) gains a second source.

- [ ] **Step 1: Read the current init path**

`fuse_client.rs:83` reads `vfs_env::RING_SECTION` and returns
`FuseInitError::NotConfigured` when unset, then line 202 does
`SharedMapping::open(section, ring_bytes)`. Understand `FuseInitError`'s variants
before adding one.

- [ ] **Step 2: Add the mode**

Resolution order, and it matters: **`VFS_RING_PATH` wins when set.** A session
configured for the Wine path must not silently fall back to a named section that
happens to exist. If neither is set, keep returning `NotConfigured` exactly as
today.

In file-backed mode:
- `SharedMapping::open_file_backed(Path::new(&path), ring_bytes)`;
- `notify_server` and `notify_slot_free` become **no-ops**. There is no
  Windows event that a native Linux Director could wait on, so signalling one
  would be a lie that costs a syscall. Carry the reasoning from this plan's
  "Why `notify_server` becomes a no-op" section into a comment there — including
  that the Linux Director spins instead, and that making it sleep would
  reintroduce the 15.6 ms stall the existing doc records.

Implement the notifier choice without duplicating `FuseClient`: an enum or an
`Option<HANDLE>` inside `WakeServerSpinClient` (null already means "do not
signal", per its existing null check) is enough. Prefer the smallest change that
keeps one code path.

- [ ] **Step 3: Test what can be tested on Windows**

A full cross-boundary test is Task 4. Here, pin the selection logic on Windows:
with `VFS_RING_PATH` pointing at a file a Director-less test creates and
`ring::init`s itself, the shim's client must open **that file** and not consult
`VFS_RING_SECTION`. Set both and assert the path wins.

Env vars are process-global, so use whatever serialisation the existing shim
tests use for env manipulation — check `crates/vfs-embed`'s
`LAUNCH_ENV_LOCK` or similar for the established pattern rather than adding a
second one.

- [ ] **Step 4: Verify and commit**

Workspace scope on Windows, plus clippy.

```bash
git add rust/crates/vfs-shim
git commit -m "feat(shim): open the ring from a file when VFS_RING_PATH is set"
```

---

### Task 4: A Wine-hosted fixture served by a Linux Director

**Files:**
- Create: `rust/crates/vfs-director/src/bin/vfs-serve-fb.rs` (a small harness
  server; `#![cfg(unix)]` with a `fn main() {}` fallback)
- Modify: `rust/crates/vfs-director/Cargo.toml` (the `[[bin]]`)

**Interfaces:** Consumes Tasks 1-3.

**This is the increment's definition of done.** Everything before it is either
same-platform or synthetic; this is the first time the **real shim**, injected
into a **real Windows process under Proton**, is served by a **native Linux
Director**.

- [ ] **Step 1: The harness server**

`vfs-serve-fb <ring-path> <root-dir> <backing-file>`: build a `Director` with one
disk-backed entry (`data/hello.txt` → the backing file), call
`IpcServe::start_file_backed`, print `SERVE: ready <ring-path>` and flush, then
run until killed. Print one line per request served so the run is legible.

- [ ] **Step 2: Drive it under Wine**

The Wine environment exists: GE-Proton at
`/root/aether/runtimes/GE-Proton11-6-x86_64`,
`WINEPREFIX=/root/aether/probe-prefix`, and `$WINEPREFIX/drive_c/probe` is
`C:\probe` inside Wine.

The client is a **Windows** process with the shim installed, reading
`C:\<root>\data\hello.txt` — a path that does not exist on disk. Use the shim's
existing in-process install (as `tests/hook_identity.rs` does) rather than
injection, so this task tests the *ring*, not the injector; injection is already
proven separately. Build that fixture on Windows, copy it into the prefix.

Env for the Wine side: `VFS_RING_PATH` set to the **Windows** view of the ring
file (`C:\probe\ring.bin`), `VFS_RING_BYTES` matching the server's, plus whatever
`fuse_client.rs` requires to consider itself configured — read it and set exactly
those.

Expected: the fixture reads bytes that exist only in the Linux Director's
provider, and the server logs the GETATTR/READ it answered.

- [ ] **Step 3: Report the literal output of both sides**

Capture exit codes **without** reading `$?` after a pipe. If it does not work,
the diagnosis is worth more than a fix: report which side logged what, and
whether the ring geometry matched.

- [ ] **Step 4: Commit**

```bash
git add rust/crates/vfs-director
git commit -m "test(director): a Wine-hosted shim served by a native Linux Director"
```

---

## Self-Review

**Spec coverage.** §3's transport selection is Tasks 1-3; §3's "an embedder gets
the right delivery for their platform" is *not* here — that is `Session::launch`,
next increment, and this plan's scope note says so. §5's prefix construction and
drive rerouting are likewise deferred.

**Type consistency.** `RingMapping` is the single alias both constructors use.
`start_file_backed` mirrors `start`'s geometry arithmetic deliberately, because
the client computes the same numbers from `VFS_RING_BYTES`. `vfs_env::RING_PATH`
is defined in Task 1 and read by Tasks 2-4.

**Known soft spots.** Task 2's test has elided client-construction sections
pointing at two existing files to copy from, rather than full code — the
`RingClient`/`SpinNotifier` idiom must be read from the source, and inventing a
second one would be worse than the elision. Task 3's env-serialisation pattern is
named as "find the existing one" for the same reason. Task 4 is inherently
integration work and its Step 2 env list says to read `fuse_client.rs` for the
exact requirements rather than trusting a list I would have guessed.
