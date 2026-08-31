# aether-vfs — Linux portability: make the Director OS-agnostic

**Goal:** `vfs-director` — the userspace FUSE kernel — compiles and tests on
Linux, so that a FUSE delivery adapter and a Proton launch runtime can be built
against it later without touching the Windows shim path. This spec covers that
enabling refactor in full, and fixes the target architecture the refactor aims
at so the two increments cannot drift.

**Status:** proposed, 2026-08-31.

---

## 1. Why

The project already had a Linux path and deleted it. Commit `15d16fb`
(2026-08-11, *"feat(rust): pure-Rust director path with Steam-stable Skyrim
hollow load"*) replaced the Clojure director stack with the Rust crates, and the
Clojure Linux adapters went with it:

| deleted file | lines | what it did |
|---|---|---|
| `src/aether/vfs/os/linux/fuse.clj` | 185 | jnr-fuse adapter: FUSE ops → Provider calls |
| `src/aether/vfs/os/linux/proton.clj` | 85 | build `proton run`, spawn with logs, grep, teardown |
| `src/aether/vfs/os/linux/launch.clj` | 62 | launch glue |

**~330 lines.** The Linux side was never a subsystem; it was a thin adapter over
an abstraction that did the real work. That abstraction is what survived the port
to Rust, which is why re-adding Linux is tractable rather than a rewrite.

The intent is also already on record.
`docs/superpowers/specs/2026-07-26-unified-cross-platform-vfs-design.md` — "one
interface, two OS-specific delivery adapters" — remains the correct shape. Only
its premise changed: the consumer is no longer Clojure/JVM but Rust, with a
TypeScript binding over it.

**The seam already exists.** `Director`'s public API *is* the FUSE operation set,
and it already speaks errno:

```
getattr  readdir  open  read  close  write
set_len  flush  mkdir  remove  rename  set_attr    // all -> Result<_, i32>
```

`Result<_, i32>` being errno rather than NTSTATUS is not an accident of this
refactor — the kernel was always POSIX-shaped, and NTSTATUS translation happens
out at the shim. `ring_dispatch::dispatch_director` is a pure translation layer
(opcode + bytes → Director call → bytes). A FUSE dispatcher is its sibling, not a
rework. `vfs-director/src/director.rs:1` has said *"Userspace FUSE kernel"* the
whole time.

**What actually blocks it** is three dependency edges, not the design. Measured,
not assumed: only four crates touch `windows-sys` (`vfs-win`, `vfs-shim`,
`vfs-shim-dll`, `vfs-inject`), and fifteen are already portable. The Director is
tangled to Windows by:

1. `vfs-shim`, for **one two-line path helper**;
2. `vfs-inject`, for **pure byte parsing** that already compiles for Linux;
3. `vfs-win`, for the shared-memory ring — the one genuinely Windows-only edge.

The first is the sharpest finding. `vfs-director/src/lib.rs:42` is
`pub use vfs_shim::overlay_layer_dir;`, and that function is:

```rust
pub fn overlay_layer_dir(overlay_root: &Path, root: RootId) -> PathBuf {
    overlay_root.join(format!("root-{}", root.0))
}
```

Two lines of path joining. Through it, the Director's dependency graph acquires
`retour` and therefore `libudis86-sys` — a **C x86 disassembler**, needed for
inline hook trampolines and needed by nothing the kernel does. It is the first
thing that fails a Linux build, and the cheapest thing in this spec to fix.

## 2. What changes

Four moves. Each severs one edge.

| # | move | severs | note |
|---|---|---|---|
| 1 | `overlay_layer_dir` → `vfs-provider` (`path.rs`); `vfs-shim` re-exports it | `vfs-shim` | kills `retour` + `libudis86-sys` from the graph |
| 2 | PE parsing → new crate `vfs-pe`; `vfs-inject` re-exports | `vfs-inject` | already proven to compile for Linux |
| 3 | `ipc.rs`, `ring_dispatch.rs` behind `cfg(windows)` | `vfs-win` | ring is Windows-only by nature |
| 4 | `vfs-win` → `[target.'cfg(windows)'.dependencies]` | — | that manifest section already existed for `windows-sys`; this increment only adds `vfs-win` to it |

Move 2 covers exactly the three functions `stage.rs` calls —
`pe_looks_like_image`, `import_dll_names_of_pe`, `is_system_import_dll`
(`stage.rs:358,382,392,425`). These parse PE bytes and call no OS API; the only
Linux errors in `vfs-inject` are in `inject.rs`
(`use std::os::windows::ffi::OsStrExt`), which stays Windows-only.

**Both moves re-export from their original homes, so no call site changes.** That
is the mechanism by which Windows stays byte-identical: the shim, the daemon and
the tests keep importing the paths they already import.

A new crate rather than merely gating `inject.rs`: a kernel depending on
something named `vfs-inject` to parse bytes is the smell that hid this coupling
for three weeks. `vfs-pe` names what it is, and it is independently testable on
both platforms.

## 3. Why `cfg(target_os)` and not features

The delivery mechanism is selected by target, automatically:

```rust
#[cfg(windows)]                 mod ipc;            // SharedMapping + EventNotifier
#[cfg(windows)]                 mod ring_dispatch;  // opcode -> Director
#[cfg(target_os = "linux")]     mod fuse_dispatch;  // FUSE op -> Director  (increment 2)

mod director;   // portable kernel, always built
mod stage;      // portable, via vfs-pe
```

An embedder writes `vfs_embed::Session::new(..)` and gets the right delivery for
their platform with no feature flags to reason about. Cargo features were
considered and rejected: they push a matrix (`ring` / `fuse` / both / neither)
onto both the embedder and CI in exchange for combinations nobody needs — the
Linux path *replaces* the ring rather than running beside it. Abstracting
`SharedMapping`/`EventNotifier` behind traits so the ring itself becomes portable
was also rejected, for the same reason: FUSE is the Linux transport, so a
portable ring would be built and never used.

Note that the ring *protocol* is already portable and stays that way —
`vfs-protocol` and `vfs-ipc` compile and test on Linux today. Only the OS handles
are gated.

## 4. Verification

Much is verifiable **locally**, which was not true when this work was first
scoped — but not everything, and the limit matters:

- **`cargo check --target x86_64-unknown-linux-gnu`** needs no cross-linker,
  because `check` does not link. Confirmed working on the Windows dev machine
  after `rustup target add x86_64-unknown-linux-gnu`.
- **It fails today** for `vfs-director`, on exactly two of the three edges:
  `libudis86-sys`'s build script (a C cross-compile, reached via `vfs-shim`) and
  `E0433`/`E0599` in `vfs-inject`'s `inject.rs` (`use std::os::windows::ffi::OsStrExt`).

**Corrected 2026-08-31, during implementation:** an earlier draft of this section
called that check "the red test" and treated it as proof of portability. It is
not, and the correction is load-bearing for increment 2. **`cargo check` cannot
detect a `windows-sys`-based dependency at all.** Demonstrated directly:
`cargo check --target x86_64-unknown-linux-gnu -p vfs-win` **succeeds** — a crate
whose entire purpose is Windows shared-memory and event handles type-checks
cleanly for Linux, because `windows-sys` emits extern declarations that type-check
on any target and only fail at *link* time.

So the check catches one specific and valuable error class — Rust-level
portability breaks like `std::os::windows` paths and missing items — and is blind
to the rest. Three gates are needed, and only the first is local:

| gate | proves | where |
|---|---|---|
| `cargo check --target x86_64-unknown-linux-gnu -p vfs-director` | no Rust-level portability breaks | local |
| `cargo tree -p vfs-director --target x86_64-unknown-linux-gnu` names no Windows crate | no Windows crate anywhere in the graph for that target, dev-dependencies included — the structural gate, and the one that actually holds | local |
| `cargo test -p vfs-director` on a real Linux host | it builds, links and the kernel's unit tests pass | CI |

**`--target` on the tree query is the whole trick, and omitting it silently
inverts the result.** `cargo tree` resolves for the **host** target by default, so
on a Windows machine both `[target.'cfg(windows)'.dependencies]` and
`[target.'cfg(windows)'.dev-dependencies]` resolve and the query reports Windows
crates even when the property holds perfectly. Measured on this tree after the
gating landed:

```
cargo tree -p vfs-director                                        -> vfs-win, windows-sys, retour, libudis86-sys
cargo tree -p vfs-director --target x86_64-unknown-linux-gnu      -> none
```

Naming the target is therefore both stricter and simpler than filtering edge
kinds with `-e normal`: it covers normal and dev dependencies in one query and
needs no caveat about which tables resolve where.
- **Windows regression:** `cargo test --no-fail-fast` and
  `cargo clippy --all-targets -- -D warnings`, both already green.
- **CI:** extend the existing `rust-linux-portable` job to include
  `vfs-director`, `vfs-pe`, `vfs-source` and `vfs-zip`, so Linux-cleanliness is
  enforced on every push instead of observed once.

WSL is present on the dev machine as a fallback for actually *running* Linux
tests, which increment 2 will need for `/dev/fuse`. It is not needed for this
increment.

## 5. What must not regress

- **No behaviour change on Windows.** This is a dependency refactor; every moved
  symbol keeps its old import path via re-export.
- **No interface change.** `vfs-embed::Session` and the `vfs-node` TypeScript
  surface are untouched. `vfs-node` depends only on `vfs-embed`, so the eventual
  payoff — one interface, two implementations — costs the TS consumer nothing,
  but nothing about it moves in this increment.
- **The protocol descriptor must not drift.** `bin/regen-protocol` and the
  `git diff --exit-code resources/` gate stay green; nothing here touches wire
  format.
- **Nothing reaches `master` until Windows is proven green.** Work happens on
  `worktree-linux-fuse-proton` in an isolated worktree so other local consumers
  of `C:\oss\aether-vfs` — including the separate checkout at
  `C:/tmp/aethervfs-aug15` — are unaffected.

## 6. Scope

**In scope:** the four moves, the new `vfs-pe` crate, the CI job extension, and
the tests that pin the moved functions in their new homes.

**Out of scope, and deliberately so:**

- `vfs-fuse` — the FUSE server. Increment 2.
- `vfs-proton` — the launch runtime. Increment 3.
- `Session` parity, so `launch()` means "stage + inject + ring" on Windows and
  "mount + Proton" on Linux. Increment 4.
- A Linux build of the Node addon.
- The `§6b` casefold hole. It is pinned by a deliberate `test.fails` in
  `primitives.test.mts` and becomes more load-bearing on Linux (see Risks), but
  closing it is its own change.

## 7. Risks

Risks to *this* increment are small; the ones worth recording are the ones this
increment must not foreclose.

**For this increment:**

- **Move 1's destination is `vfs-provider`, not `vfs-core`, and the reason
  matters.** `overlay_layer_dir` takes a `RootId`, and `vfs-core` depends on
  nothing but `blake3` — hosting the signature there would mean giving the
  foundational leaf crate a new dependency. `vfs-provider` *defines* `RootId`
  (`path.rs:7`), is already portable and Linux-tested in CI, and is already a
  `vfs-director` dependency, so the move adds no edge anywhere. There is exactly
  one `RootId` in the workspace; `vfs-redirect`, `vfs-protocol`, `vfs-director`
  and `vfs-embed` all re-export that one type, so no type-identity ambiguity
  arises from the move.
- **`libudis86-sys` failing to build for Linux is a cross-compilation symptom,
  not only a portability one** — there is no C cross-compiler here. Move 1
  removes it from the graph entirely, so the distinction stops mattering; it
  should not be mistaken for a fix to the C build.

**For the increments after it:**

- **Case folding is the real semantic fork.** Everything folds vpaths through
  `vfs-core::fold` because Windows is case-insensitive. On Linux the adapter
  either folds (matching Windows exactly, keeping the existing suite meaningful)
  or stays case-sensitive and lets Wine resolve case by scanning directories —
  a readdir per lookup miss. This spec does not decide it. Nothing here needs to:
  `fold` lives in `vfs-core`, which is already portable and which no move in this
  increment touches, so both adapters can reach it either way.
- **FUSE loses the shim's observability.** Hooking NT APIs in-process sees the
  game's literal request; under Proton, Wine canonicalizes and caches first. The
  `skyrim-empty-load-order` finding (CWD-relative opens being undecodable) was
  this class of problem, and the Linux path changes *what is observable*, not
  merely where.
- **`NtCreateSection`/`MapView` disk-preference does not port as-is.** `15d16fb`
  deliberately prefers real disk handles for ESM/BSA. Under FUSE, mmap pages in
  through the FUSE read path, so the tier-3 benchmark ceilings — set against the
  ring — will not transfer.
- **No zero-copy read in the same shape.** `fuse.clj` used FFM to write straight
  into the kernel's FUSE buffer (`ReadInto`); `fuser`'s `reply.data()` copies.
- **`fuser` is inode-based, jnr-fuse was path-based.** libfuse's high-level API
  resolved inodes for the Clojure adapter, so the Rust one must carry an
  inode↔vpath table. `fuse.clj`'s own comment noted `aether.vfs.inode` was kept
  "for a future low-level adapter" — that future is increment 2. Also port its
  concurrency bound: a semaphore capping concurrent reads (default 32), because
  libfuse spawns a thread per in-flight request and slow providers pile up faster
  than they drain.

## 8. Definition of done

1. `cargo check --target x86_64-unknown-linux-gnu -p vfs-director` succeeds.
   Necessary, not sufficient — see the correction in section 4 for why this alone
   proves less than it appears to.
2. `cargo tree -p vfs-director --target x86_64-unknown-linux-gnu` contains none of
   `retour`, `libudis86-sys`, `vfs-win`, `vfs-shim`, `vfs-inject` or
   `windows-sys`. This is the structural gate and the one that actually holds.
   **The `--target` flag is mandatory** — without it `cargo tree` resolves for the
   Windows host, both `cfg(windows)` tables resolve, and the query reports Windows
   crates even though the property holds. See section 4.
3. `cargo test --no-fail-fast` on Windows is green, with no test edited to make
   it so.
4. `cargo clippy --all-targets -- -D warnings` is clean.
5. `bin/regen-protocol` produces no diff under `resources/`.
6. `rust-linux-portable` in CI covers `vfs-director` and `vfs-pe`. Unproven on
   this machine: it has no cross-linker, so `cargo test --target
   x86_64-unknown-linux-gnu -p vfs-director --no-run` dies with `linker 'cc'
   not found`. `cargo test -p vfs-director` has **never actually run** on
   Linux here — only type-checked (`cargo check --target
   x86_64-unknown-linux-gnu`, item 1). This item is unproven until a CI push
   confirms it.
7. `vfs-embed`'s and `vfs-node`'s public surfaces are byte-identical to
   `f0a55ef`.
