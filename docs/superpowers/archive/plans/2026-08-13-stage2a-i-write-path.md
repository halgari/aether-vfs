# Stage 2a-i: The Write Path — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the director able to serve writes end to end, so that phase 2a-ii can delete the shim's write fall-through without breaking the game.

**Architecture:** The conformance suite gains its `ReadWrite` half first, so every provider that claims write access is held to one standard. `DiskProvider` then implements writes, `OverlayProvider` gains copy-up and is promoted to `ReadWrite`, the `Director` accepts `OPEN_WRITE` and owns append cursors, `ring_dispatch` gains the five reserved write opcodes, and the shim's `NtSetInformationFile` stops silently succeeding on delete and rename.

**Tech Stack:** Rust 2021, Windows NT API, shared-memory ring IPC.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-08-13-no-bypass-and-real-roots-design.md`. Read §3 and §7 before starting. Provider contract: `docs/superpowers/specs/2026-08-13-pluggable-providers-design.md` §5 and §7.
- Working directory for all cargo commands is `C:\oss\aether-vfs\rust`.
- Zero clippy warnings: `cargo clippy --all-targets -- -D warnings` clean at every commit.
- `vfs-payload` is a **separate workspace**: `cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target`.
- Baseline before this plan: `cargo test --workspace` = 357 passed, 0 failed, 1 ignored. Every task should raise the passing count and must never lower it.
- **This phase does NOT close any bypass.** Do not delete `Decision::Redirect`, `Decision::Serve`, the DRM exceptions, or the shim's write fall-through. That is phase 2a-ii. Removing them here makes a write-path failure indistinguishable from a bypass-closing failure, which is the entire reason for the split.
- Status codes are fixed by the wire protocol. `ST_NOT_SUPPORTED = -8` and `ST_READ_ONLY = -9` already exist.
- **Every write wire codec already exists** in `vfs-protocol/src/lib.rs`: `encode/decode_write_req`, `encode/decode_write_resp`, `encode/decode_mkdir_req`, `encode/decode_rename_req`, `encode/decode_setattr_req`. Do not invent new ones or change their layout — the shim client already speaks them.
- Tests use the existing isolation convention: temp dirs named with `std::process::id()`, cleaned up at the end.
- Conventional commit prefixes. Commit after every task.

---

## File Structure

**Modified:**

| File | Change |
|---|---|
| `crates/vfs-provider/src/conformance.rs` | `assert_writable` cases; `RwMemFixture` |
| `crates/vfs-provider/src/lib.rs` | Export `RwMemFixture` |
| `crates/vfs-director/src/disk.rs` | `DiskProvider` write half |
| `crates/vfs-compose/src/overlay.rs` | Copy-up, whiteouts, `ReadWrite` |
| `crates/vfs-director/src/director.rs` | `OPEN_WRITE`, append cursors, write routing, `ST_READ_ONLY` |
| `crates/vfs-director/src/ring_dispatch.rs` | `OP_WRITE`, `OP_SETATTR`, `OP_RENAME`, `OP_DELETE`, `OP_MKDIR` |
| `crates/vfs-director/src/io_stats.rs` | Write and rejected-write counters |
| `crates/vfs-cache/src/provider.rs` | **Write-transparent cache**: forward the write half, invalidate on `write_at`/`set_len`, and drop the `OPEN_WRITE` rejection |
| `crates/vfs-shim/src/hook.rs` | `NtSetInformationFile` delete/rename, `NtFlushBuffersFile` |

**Created:** `crates/vfs-fixture-writepath/` (an end-to-end fixture executable).

**A gap this plan originally missed.** `SessionRegistry::add_source` — the only production path, used by both the gRPC daemon and the end-to-end harness — wraps **every** mounted backend in `CachingProvider`. Its `open()` rejected `OPEN_WRITE` while its `capabilities()` forwarded the inner provider's `ReadWrite`, so writes through the real path were refused and then fell through to the shim's overlay redirect: the exact bypass this stage exists to close. No unit test could see it, because unit tests mount providers directly and skip the registry.

The systematic guard, which belongs in whichever task touches `vfs-cache`: run `assert_conformance` over a `CachingProvider` wrapping a **writable** inner provider. Stage 1 added a cached-provider conformance test, but its inner was read-only, so the write cases never ran. That one test would have caught this before the end-to-end test did.

---

### Task 1: The `ReadWrite` half of the conformance suite

**Files:**
- Modify: `crates/vfs-provider/src/conformance.rs`, `crates/vfs-provider/src/lib.rs`

**Interfaces:**
- Consumes: `Provider`, `Capabilities`, `Access`, `VPath`, `SetAttr`, the `OPEN_*` flags.
- Produces: `assert_writable(&Arc<dyn Provider>)` called from `assert_conformance` when `access == ReadWrite`; `pub struct RwMemFixture` — an in-memory `ReadWrite` reference provider.

**Why first:** every provider in Tasks 2 and 3 claims `ReadWrite`. Without the cases, those claims are unverified and the ports get rubber-stamped — the exact failure the Stage 1 review caught in the read half.

**Design constraint that matters:** write cases **mutate** the provider, and the read cases assert an exact fixture tree. So write cases run **last**, and every path they touch uses a `w_` prefix that is absent from `FIXTURE_FILES`. A write case must never disturb `a.txt` or `sub/b.txt`.

- [ ] **Step 1: Write the failing tests**

Add to `conformance.rs`'s test module:

```rust
    #[test]
    fn the_writable_fixture_passes_its_own_suite() {
        assert_conformance(std::sync::Arc::new(RwMemFixture::new()));
    }

    #[test]
    #[should_panic(expected = "read back")]
    fn a_provider_whose_writes_vanish_fails_the_suite() {
        assert_conformance(std::sync::Arc::new(RwMemFixture::discarding_writes()));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-provider --lib conformance`
Expected: compile error — `RwMemFixture` is not defined.

- [ ] **Step 3: Implement `RwMemFixture`**

Add to `conformance.rs`. Delegate `getattr`/`readdir` to an inner `MemFixture` for the immutable base tree, and keep written files in a separate map so the base tree is never mutated. `discarding_writes` accepts writes and drops them, to prove the suite detects it.

```rust
/// In-memory `ReadWrite` reference provider. The `FIXTURE_FILES` tree is served
/// read-only from an inner `MemFixture`; written paths live in `extra`, so the
/// read cases keep seeing the exact reference tree.
pub struct RwMemFixture {
    base: MemFixture,
    extra: Mutex<HashMap<String, Vec<u8>>>,
    dirs: Mutex<Vec<String>>,
    next: AtomicU64,
    opens: Mutex<HashMap<Handle, String>>,
    discard: bool,
}

impl RwMemFixture {
    pub fn new() -> Self {
        Self::build(false)
    }

    /// Accepts writes and drops them — proves the suite catches a provider
    /// whose writes do not stick.
    pub fn discarding_writes() -> Self {
        Self::build(true)
    }

    fn build(discard: bool) -> Self {
        RwMemFixture {
            base: MemFixture::new(),
            extra: Mutex::new(HashMap::new()),
            dirs: Mutex::new(Vec::new()),
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
            discard,
        }
    }
}

impl Default for RwMemFixture {
    fn default() -> Self {
        Self::new()
    }
}
```

Implement `Provider` for it: `capabilities` returns `Capabilities { access: Access::ReadWrite, immutable: false, slow: false, preferred_block: None }`; `getattr`/`readdir` consult `extra` and `dirs` first, then delegate to `self.base`; `open` honours `OPEN_CREATE`/`OPEN_EXCL`/`OPEN_TRUNC` against `extra`; `read_at` serves from `extra` or delegates; `write_at` splices into the `extra` entry (unless `discard`); `set_len` truncates or zero-extends; `flush` is a no-op returning `Ok(())`; `mkdir` pushes to `dirs`; `remove` drops from `extra`/`dirs`; `rename` moves the entry; `set_attr` is a no-op returning `Ok(())`.

- [ ] **Step 4: Implement the write cases**

Add to `conformance.rs`, and call it from `assert_conformance` after the read cases:

```rust
pub fn assert_conformance(p: Arc<dyn Provider>) {
    // ... existing read dispatch ...
    if caps.access == Access::ReadWrite {
        assert_writable(&p);   // last: these cases mutate
    }
}
```

```rust
/// Write cases. Run last, because they mutate. Every path is `w_`-prefixed so
/// the reference tree the read cases assert is never disturbed.
fn assert_writable(p: &Arc<dyn Provider>) {
    use crate::{OPEN_APPEND, OPEN_CREATE, OPEN_EXCL, OPEN_TRUNC, OPEN_WRITE};

    let f = VPath::at_default("w_new.txt");

    // Create, write, read back through a fresh handle.
    let (h, _, _) = p.open(f, OPEN_WRITE | OPEN_CREATE).expect("open create");
    assert_eq!(p.write_at(h, 0, b"hello").expect("write_at"), 5);
    p.flush(h).expect("flush");
    p.close(h).expect("close");

    let st = p.getattr(f).expect("getattr after write").expect("file must exist after write");
    assert_eq!(st.size, 5, "size after write");

    let (h, size, _) = p.open(f, crate::OPEN_READ).expect("reopen for read");
    assert_eq!(size, 5);
    let mut buf = [0u8; 8];
    let n = p.read_at(h, 0, &mut buf).expect("read_at");
    assert_eq!(&buf[..n], b"hello", "written bytes did not read back");
    p.close(h).expect("close");

    // EXCL refuses an existing path.
    assert!(
        p.open(f, OPEN_WRITE | OPEN_CREATE | OPEN_EXCL).is_err(),
        "OPEN_EXCL must fail on an existing file"
    );

    // TRUNC empties it.
    let (h, _, _) = p.open(f, OPEN_WRITE | OPEN_TRUNC).expect("open trunc");
    p.close(h).expect("close");
    assert_eq!(p.getattr(f).expect("getattr").expect("exists").size, 0, "TRUNC must empty the file");

    // Positional overwrite mid-file.
    let (h, _, _) = p.open(f, OPEN_WRITE).expect("open write");
    p.write_at(h, 0, b"abcdef").expect("write_at");
    p.write_at(h, 2, b"XY").expect("overwrite");
    p.close(h).expect("close");
    let (h, _, _) = p.open(f, crate::OPEN_READ).expect("reopen");
    let n = p.read_at(h, 0, &mut buf).expect("read_at");
    assert_eq!(&buf[..n], b"abXYef", "positional overwrite wrong");
    p.close(h).expect("close");

    // set_len shrinks and grows; growth zero-fills.
    let (h, _, _) = p.open(f, OPEN_WRITE).expect("open");
    p.set_len(h, 2).expect("shrink");
    p.set_len(h, 4).expect("grow");
    p.close(h).expect("close");
    let (h, size, _) = p.open(f, crate::OPEN_READ).expect("reopen");
    assert_eq!(size, 4, "set_len size wrong");
    let n = p.read_at(h, 0, &mut buf).expect("read_at");
    assert_eq!(&buf[..n], b"ab\0\0", "set_len growth must zero-fill");
    p.close(h).expect("close");

    // Append lands at end of file.
    let (h, _, _) = p.open(f, OPEN_WRITE | OPEN_TRUNC).expect("open trunc");
    p.write_at(h, 0, b"one").expect("write");
    p.close(h).expect("close");
    let (h, _, _) = p.open(f, OPEN_WRITE | OPEN_APPEND).expect("open append");
    p.write_at(h, 3, b"two").expect("append");
    p.close(h).expect("close");
    let (h, _, _) = p.open(f, crate::OPEN_READ).expect("reopen");
    let n = p.read_at(h, 0, &mut buf).expect("read_at");
    assert_eq!(&buf[..n], b"onetwo", "append did not land at end");
    p.close(h).expect("close");

    // mkdir is visible to getattr and readdir.
    let d = VPath::at_default("w_dir");
    p.mkdir(d).expect("mkdir");
    let st = p.getattr(d).expect("getattr dir").expect("dir must exist");
    assert_eq!(st.kind, crate::KIND_DIR, "mkdir did not produce a directory");
    assert!(
        p.readdir(VPath::at_default(""))
            .expect("readdir root")
            .iter()
            .any(|e| e.name == "w_dir"),
        "mkdir not visible in readdir"
    );

    // rename moves content and clears the old name.
    let g = VPath::at_default("w_moved.txt");
    p.rename(f, g).expect("rename");
    assert!(p.getattr(f).expect("getattr old").is_none(), "rename left the old name behind");
    let st = p.getattr(g).expect("getattr new").expect("renamed file must exist");
    assert_eq!(st.size, 6, "rename lost content");

    // Cross-root rename is refused.
    assert_eq!(
        p.rename(g, VPath::new(RootId(9), "w_moved.txt")),
        Err(crate::bad_request()),
        "cross-root rename must be refused"
    );

    // remove clears a file and an empty directory.
    p.remove(g).expect("remove file");
    assert!(p.getattr(g).expect("getattr removed").is_none(), "remove did not delete the file");
    p.remove(d).expect("remove dir");
    assert!(p.getattr(d).expect("getattr removed dir").is_none(), "remove did not delete the dir");

    // set_attr accepts an mtime without error.
    let keep = VPath::at_default("w_attr.txt");
    let (h, _, _) = p.open(keep, OPEN_WRITE | OPEN_CREATE).expect("open create");
    p.close(h).expect("close");
    p.set_attr(keep, crate::SetAttr { mtime: Some(1_700_000_000), size: None })
        .expect("set_attr mtime");
    p.remove(keep).expect("cleanup");

    // The reference tree survived: write cases must not disturb it. Compare
    // bytes, not just size — a same-length scribble is the corruption this
    // check exists to catch, and a size comparison cannot see it.
    for (rel, body) in FIXTURE_FILES {
        let vp = VPath::at_default(rel);
        let st = p
            .getattr(vp)
            .unwrap_or_else(|e| panic!("getattr({rel}) after writes failed with {e}"))
            .unwrap_or_else(|| panic!("write cases destroyed {rel}"));
        assert_eq!(st.size, body.len() as u64, "write cases altered {rel}'s size");

        let (h, _, _) = p
            .open(vp, crate::OPEN_READ)
            .unwrap_or_else(|e| panic!("reopen({rel}) after writes failed with {e}"));
        let got = read_all(p, h, st.size);
        p.close(h).expect("close");
        assert_eq!(got, *body, "write cases altered {rel}'s content");
    }
}
```

- [ ] **Step 5: Export the fixture**

In `lib.rs`, add `RwMemFixture` to the `conformance` re-export list.

- [ ] **Step 6: Run to verify it passes**

Run: `cargo test -p vfs-provider`
Expected: 20 passed (18 prior + 2 new). Both new tests must pass — in particular `a_provider_whose_writes_vanish_fails_the_suite`, which proves the write cases are not vacuous.

Run: `cargo clippy -p vfs-provider --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add rust/crates/vfs-provider
git commit -m "test(provider): the ReadWrite half of the conformance suite

Write cases run last and use w_-prefixed paths so the reference tree the
read cases assert is never disturbed. Includes a discarding-writes
fixture proving the cases are not vacuous."
```

---

### Task 2: `DiskProvider` writes

**Files:**
- Modify: `crates/vfs-director/src/disk.rs`

**Interfaces:**
- Consumes: `assert_writable` via `assert_conformance` (Task 1).
- Produces: `DiskProvider` declaring `Access::ReadWrite` and passing full conformance.

- [ ] **Step 1: Write the failing test**

Replace the capability assertion in `disk.rs`'s test module (it currently asserts `Access::Read` with the comment "writes arrive in Stage 3") and add a conformance test over a writable tree:

```rust
    #[test]
    fn disk_provider_declares_read_write() {
        use vfs_provider::{Access, Provider};
        let dir = std::env::temp_dir().join(format!("vfs-diskrw-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);
        let p = DiskProvider::new(&dir);
        let caps = p.capabilities();
        assert_eq!(caps.access, Access::ReadWrite);
        assert!(!caps.immutable, "a real directory can change underneath us");
        caps.validate().expect("ReadWrite must not claim immutable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_provider_passes_write_conformance() {
        let dir = std::env::temp_dir().join(format!("vfs-diskwconf-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);
        let p: std::sync::Arc<dyn vfs_provider::Provider> = std::sync::Arc::new(DiskProvider::new(&dir));
        vfs_provider::assert_conformance(p);
        let _ = std::fs::remove_dir_all(&dir);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-director --lib disk`
Expected: FAIL — capabilities still say `Access::Read`, and the write cases hit `ST_NOT_SUPPORTED`.

- [ ] **Step 3: Implement the write half**

In `disk.rs`:

1. `capabilities()` → `access: Access::ReadWrite` (drop the "writes arrive in Stage 3" comment).
2. `open` stops rejecting `OPEN_WRITE`. Build the file with `std::fs::OpenOptions`: `.read(true)`, `.write(flags & OPEN_WRITE != 0)`, `.create(flags & OPEN_CREATE != 0)`, `.create_new(flags & OPEN_EXCL != 0)`, `.truncate(flags & OPEN_TRUNC != 0)`. When `OPEN_CREATE` is set, create missing parent directories with `std::fs::create_dir_all` on the parent — games create files in directories they expect to exist.
3. `write_at`: seek and write, mirroring `read_at`'s existing lock-and-seek shape.
4. `set_len`: `File::set_len`.
5. `flush`: `File::sync_all`, mapping errors through `map_io_err()`.
6. `mkdir`: `std::fs::create_dir_all` on the resolved path.
7. `remove`: `std::fs::remove_file`, falling back to `std::fs::remove_dir` when the target is a directory; `NotFound` maps to `not_found()`.
8. `rename`: refuse when `from.root != to.root` with `bad_request()`, then `std::fs::rename`.
9. `set_attr`: apply `mtime` with `File::set_times` when present and `size` via `set_len` when present; `None` fields leave the attribute alone.

**Note the pre-existing wart:** `getattr` and `readdir` hardcode `mtime: 0`. Do **not** fix that here — it is out of scope and changing it would alter cache-key inputs. Record it in your report.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p vfs-director`
Expected: all pass, including the two new tests.

Run: `cargo clippy -p vfs-director --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-director/src/disk.rs
git commit -m "feat(director): DiskProvider implements the write contract"
```

---

### Task 3: `OverlayProvider` copy-up

**Files:**
- Modify: `crates/vfs-compose/src/overlay.rs`

**Interfaces:**
- Consumes: `Provider` write methods (Task 2 shape), `assert_writable`.
- Produces: `OverlayProvider` declaring `ReadWrite`, with copy-up, whiteouts, and opaque-directory semantics.

**Semantics** (spec §7 of the providers design):

| Operation | Behaviour |
|---|---|
| `open` write, present in upper | Open in upper |
| `open` write, whiteout present | Not found, unless `OPEN_CREATE` |
| `open` write, present in base | **Copy the whole file up**, then open in upper |
| `open` write, in neither | Create in upper if `OPEN_CREATE`, else not found |
| `remove`, in upper | Delete in upper |
| `remove`, visible in base | Write a `.wh.<name>` whiteout in upper |
| `remove` on a base directory | Whiteout hides the whole subtree |
| `rename` | Copy up source, rename in upper, whiteout the original |

Copy-up is **whole-file, not lazy per block** — the files games write are INIs, saves, and logs; the multi-gigabyte files are read-only.

**`upper` must declare `ReadWrite`**; validate at construction and fail there, not at first write.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn overlay_declares_read_write_over_a_read_only_base() {
        use vfs_provider::{Access, Provider};
        let base = Arc::new(InlineProvider::from_files(vfs_provider::FIXTURE_FILES.iter().copied()));
        let dir = std::env::temp_dir().join(format!("vfs-ovrw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ov = OverlayProvider::new(base, MemUpper::default()).unwrap();
        assert_eq!(ov.capabilities().access, Access::ReadWrite);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overlay_rejects_a_read_only_upper_at_construction() {
        let base = Arc::new(InlineProvider::from_files(vfs_provider::FIXTURE_FILES.iter().copied()));
        let upper = Arc::new(InlineProvider::from_files(std::iter::empty::<(&str, &[u8])>()));
        assert!(
            OverlayProvider::new(base, upper).is_err(),
            "a read-only upper must be refused at construction, not at first write"
        );
    }

    #[test]
    fn writing_a_base_file_copies_it_up_and_leaves_base_untouched() {
        use vfs_provider::{Provider, VPath, OPEN_READ, OPEN_WRITE};
        let base = Arc::new(InlineProvider::from_files([("a.txt", b"BASE".as_slice())]));
        let dir = std::env::temp_dir().join(format!("vfs-ovcu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ov = OverlayProvider::new(base.clone(), MemUpper::default()).unwrap();

        let f = VPath::at_default("a.txt");
        let (h, _, _) = ov.open(f, OPEN_WRITE).expect("open for write copies up");
        ov.write_at(h, 0, b"UP").expect("write");
        ov.close(h).expect("close");

        let (h, _, _) = ov.open(f, OPEN_READ).expect("reopen");
        let mut buf = [0u8; 8];
        let n = ov.read_at(h, 0, &mut buf).expect("read");
        assert_eq!(&buf[..n], b"UPSE", "copy-up must preserve the untouched tail");
        ov.close(h).expect("close");

        // The base is never mutated.
        let (bh, _, _) = base.open(f, OPEN_READ).expect("base open");
        let n = base.read_at(bh, 0, &mut buf).expect("base read");
        assert_eq!(&buf[..n], b"BASE", "copy-up mutated the base");
        base.close(bh).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removing_a_base_file_writes_a_whiteout() {
        use vfs_provider::{Provider, VPath};
        let base = Arc::new(InlineProvider::from_files([("a.txt", b"BASE".as_slice())]));
        let dir = std::env::temp_dir().join(format!("vfs-ovwh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ov = OverlayProvider::new(base, MemUpper::default()).unwrap();
        let f = VPath::at_default("a.txt");
        ov.remove(f).expect("remove");
        assert!(ov.getattr(f).expect("getattr").is_none(), "whiteout did not hide the base file");
        assert!(
            !ov.readdir(VPath::at_default("")).expect("readdir").iter().any(|e| e.name == "a.txt"),
            "whiteout did not hide the entry from readdir"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_opens_copy_up_exactly_once() {
        use std::sync::Arc as StdArc;
        use vfs_provider::{Provider, VPath, OPEN_WRITE};
        let base = Arc::new(InlineProvider::from_files([("a.txt", b"BASE".as_slice())]));
        let dir = std::env::temp_dir().join(format!("vfs-ovrace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ov: StdArc<OverlayProvider> =
            StdArc::new(OverlayProvider::new(base, MemUpper::default()).unwrap());

        let mut hs = Vec::new();
        for _ in 0..8 {
            let ov = StdArc::clone(&ov);
            hs.push(std::thread::spawn(move || {
                let (h, _, _) = ov.open(VPath::at_default("a.txt"), OPEN_WRITE).expect("open");
                ov.close(h).expect("close");
            }));
        }
        for h in hs {
            h.join().expect("thread");
        }
        // Content must still be the base content, not a truncated or doubled copy.
        let (h, size, _) = ov.open(VPath::at_default("a.txt"), vfs_provider::OPEN_READ).unwrap();
        assert_eq!(size, 4, "concurrent copy-up corrupted the file");
        ov.close(h).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overlay_passes_write_conformance() {
        let base = Arc::new(InlineProvider::from_files(vfs_provider::FIXTURE_FILES.iter().copied()));
        let dir = std::env::temp_dir().join(format!("vfs-ovconf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ov: Arc<dyn vfs_provider::Provider> =
            Arc::new(OverlayProvider::new(base, MemUpper::default()).unwrap());
        vfs_provider::assert_conformance(ov);
        let _ = std::fs::remove_dir_all(&dir);
    }
```

**Note:** `OverlayProvider::new` changes signature — it now takes an `Arc<dyn Provider>` upper rather than a `PathBuf`, and returns `Result`.

**The upper must be a blank, test-local writable provider — not `RwMemFixture`.** `RwMemFixture` is a *conformance* fixture, permanently obligated to serve `FIXTURE_FILES` so it can pass the suite; an overlay's upper must start empty. Using it as the upper breaks copy-up tests (the upper already "contains" `a.txt`) and, worse, makes `overlay_passes_write_conformance` pass **even if the overlay ignored its base entirely** — the upper's phantom copy answers everything.

Define a `MemUpper` in `vfs-compose`'s `#[cfg(test)]` module: an in-memory `Access::ReadWrite` provider backed purely by a `files` map and a `dirs` set, starting empty. Keep it test-local — it is a test double, not public API, and `vfs-provider` stays untouched.

Do **not** reach for `vfs-director::DiskProvider` either — `vfs-director` already dev-depends on `vfs-compose`, and the reverse edge creates a dev-dependency cycle that made `--all-targets` unusable once before (see the comment in `vfs-inject/Cargo.toml` about cargo#6313).

Two assertions prove the fixture choice was right: `writing_a_base_file_copies_it_up…` must read back `"UPSE"` (4 bytes, from `"BASE"`) and not 5 bytes from a shadowing upper; and `overlay_passes_write_conformance` must fail if `OverlayProvider::getattr` is temporarily made to skip its base.

Since the tests use an in-memory upper, the whiteout and copy-up code must work through the **provider interface**, not through `std::fs` — which is the point: an in-memory upper gets deletes for free only if whiteouts are written through `upper.open`/`upper.write_at`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-compose --lib overlay`
Expected: compile error — `OverlayProvider::new` has the old signature.

- [ ] **Step 3: Implement**

Change `OverlayProvider` to hold `base: Arc<dyn Provider>` and `upper: Arc<dyn Provider>`. `new` validates `upper.capabilities().access == Access::ReadWrite` and returns `Err(&'static str)` otherwise.

`capabilities` must force **both** `access` and `immutable`:

```rust
    fn capabilities(&self) -> Capabilities {
        // A writable upper makes the stack writable regardless of the base, and
        // a stack you can write to is by definition not immutable — declaring
        // otherwise would be a promise a caching layer would act on.
        // `slow` and `preferred_block` still combine across both children.
        Capabilities {
            access: Access::ReadWrite,
            immutable: false,
            ..Capabilities::weakest([self.base.capabilities(), self.upper.capabilities()])
        }
    }
```

**Forcing `immutable: false` is required, not cosmetic.** `Capabilities::validate()` rejects `ReadWrite + immutable` as self-contradictory, and `InlineProvider` — the base in the conformance test above — declares `immutable: true`. Passing the base's `immutable` through would make `assert_conformance` panic on its own validation call.

**One pre-existing test must be rewritten**, which is the single assertion change authorised in this task: `overlay_capabilities_derive_from_base_but_clamp_access_to_read` asserts the Stage-1 read-only semantics this task supersedes, and can no longer pass under any valid construction. Rename it to `overlay_reports_read_write_and_is_never_immutable`, assert the new semantics, and comment *why* `immutable` is false so a later reader does not "fix" it back.

Whiteouts keep the `.wh.<name>` convention but are now created through the upper provider's `open`/`write_at`, so an in-memory upper gets deletes for free.

Copy-up: guard with a `Mutex<HashSet<String>>` of in-flight paths plus a re-check after acquiring, so two concurrent opens copy up exactly once.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p vfs-compose`
Expected: all pass including the six new tests.

Run: `cargo clippy -p vfs-compose --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-compose/src/overlay.rs
git commit -m "feat(compose): OverlayProvider copy-up writes and whiteouts"
```

---

### Task 4: `Director` write path

**Files:**
- Modify: `crates/vfs-director/src/director.rs`, `crates/vfs-director/src/io_stats.rs`

**Interfaces:**
- Consumes: provider write methods from Tasks 2-3.
- Produces: `Director::write(fh, offset, buf) -> Result<usize, i32>`, `Director::set_len`, `Director::flush`, `Director::mkdir`, `Director::remove`, `Director::rename`, `Director::set_attr`, all taking `&str` paths like the existing methods. `Director::open` accepts `OPEN_WRITE`.

**The director owns append.** Providers stay purely positional. `OpenRec` gains a `cursor: u64` initialised to `size` at open when `OPEN_APPEND` is set; a write on an append handle ignores the caller's offset and uses the cursor, advancing it. **Named limitation:** two handles appending to the same file concurrently can interleave incorrectly. Games write logs from one handle. Record it in a comment.

**`ST_READ_ONLY`.** When `open` is called with `OPEN_WRITE` and the resolved mount's provider declares an access level below `ReadWrite`, return `ST_READ_ONLY` — not `ST_BAD_REQUEST`. Every such rejection is recorded in `io_stats` by path with a first-seen timestamp and a count, and exposed as a getter so `vfs stats` can print it.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn open_for_write_against_a_read_only_provider_is_read_only_not_bad_request() {
        // InlineProvider is Access::Read.
        let d = Director::new();
        d.mount("/", Arc::new(vfs_compose::InlineProvider::from_files([("f", b"x".as_slice())])))
            .unwrap();
        assert_eq!(d.open("f", OPEN_WRITE), Err(vfs_provider::ST_READ_ONLY));
    }

    #[test]
    fn a_rejected_write_is_recorded_for_discovery() {
        let d = Director::new();
        d.mount("/", Arc::new(vfs_compose::InlineProvider::from_files([("f", b"x".as_slice())])))
            .unwrap();
        crate::io_stats::reset_rejected_writes();
        let _ = d.open("f", OPEN_WRITE);
        let rejected = crate::io_stats::rejected_writes();
        assert!(
            rejected.iter().any(|(path, count)| path == "f" && *count >= 1),
            "a rejected write must be discoverable, got {rejected:?}"
        );
    }

    #[test]
    fn write_then_read_through_the_director_round_trips() {
        let dir = std::env::temp_dir().join(format!("vfs-dirw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = Director::new();
        d.mount("/", Arc::new(crate::DiskProvider::new(&dir))).unwrap();

        let (fh, _, _) = d.open("w.txt", OPEN_WRITE | vfs_provider::OPEN_CREATE).unwrap();
        assert_eq!(d.write(fh, 0, b"hello").unwrap(), 5);
        d.close(fh).unwrap();

        let (fh, size, _) = d.open("w.txt", OPEN_READ).unwrap();
        assert_eq!(size, 5);
        let mut buf = [0u8; 8];
        let n = d.read(fh, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
        d.close(fh).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_handles_land_at_end_of_file() {
        let dir = std::env::temp_dir().join(format!("vfs-dira-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log.txt"), b"one").unwrap();
        let d = Director::new();
        d.mount("/", Arc::new(crate::DiskProvider::new(&dir))).unwrap();

        let (fh, _, _) = d.open("log.txt", OPEN_WRITE | vfs_provider::OPEN_APPEND).unwrap();
        // Offset 0 must be ignored on an append handle.
        d.write(fh, 0, b"two").unwrap();
        d.close(fh).unwrap();
        assert_eq!(std::fs::read(dir.join("log.txt")).unwrap(), b"onetwo");
        let _ = std::fs::remove_dir_all(&dir);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-director --lib director`
Expected: FAIL — `Director::open` rejects `OPEN_WRITE` with `bad_request()`, and `Director::write` does not exist.

- [ ] **Step 3: Implement**

Remove the `OPEN_WRITE` rejection at the top of `Director::open`. After resolving the mount, check the provider's declared access and return `read_only()` plus an `io_stats` record when the caller wants write and the provider cannot. Add `cursor: Option<u64>` to `OpenRec`, set when `OPEN_APPEND`. Add the seven pass-through methods, each resolving the mount exactly as `getattr` does and forwarding with a `VPath::at_default`.

In `io_stats.rs`, add `record_write(fh, n, err)`, `record_rejected_write(path)`, `rejected_writes() -> Vec<(String, u64)>`, and `reset_rejected_writes()`, following the existing counter conventions in that file.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p vfs-director`
Expected: all pass including the four new tests.

Run: `cargo clippy -p vfs-director --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-director/src
git commit -m "feat(director): accept OPEN_WRITE, own append cursors, record rejects"
```

---

### Task 5: Ring dispatch for the write opcodes

**Files:**
- Modify: `crates/vfs-director/src/ring_dispatch.rs`

**Interfaces:**
- Consumes: `Director` write methods (Task 4); the existing wire codecs in `vfs-protocol`.
- Produces: `dispatch_director` handling `OP_WRITE`, `OP_SETATTR`, `OP_RENAME`, `OP_DELETE`, `OP_MKDIR`.

**The codecs already exist.** Use `decode_write_req`, `encode_write_resp`, `decode_mkdir_req`, `decode_rename_req`, `decode_setattr_req` exactly as written — the shim client already encodes against them.

- [ ] **Step 1: Write the failing test**

Add to `ring_dispatch.rs`'s test module (or create one) a test that drives `dispatch_director` directly:

```rust
    #[test]
    fn write_opcode_round_trips_through_dispatch() {
        use vfs_protocol::{encode_open_req, encode_write_req, decode_open_resp, decode_write_resp,
                           WriteReq, OP_OPEN, OP_WRITE, OPEN_CREATE, OPEN_WRITE, ST_OK};
        let dir = std::env::temp_dir().join(format!("vfs-rdw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = Director::new();
        d.mount("/", std::sync::Arc::new(crate::DiskProvider::new(&dir))).unwrap();

        let (st, payload) = dispatch_director(
            &d, OP_OPEN, &encode_open_req(OPEN_WRITE | OPEN_CREATE, "w.txt"), 0, 4096, None);
        assert_eq!(st, ST_OK, "open for write must succeed through dispatch");
        let fh = decode_open_resp(&payload).unwrap().fh;

        let req = WriteReq { fh, offset: 0, len: 5 };
        let (st, payload) = dispatch_director(
            &d, OP_WRITE, &encode_write_req(&req, b"hello"), 0, 4096, None);
        assert_eq!(st, ST_OK, "write must succeed through dispatch");
        assert_eq!(decode_write_resp(&payload).unwrap(), 5);

        assert_eq!(std::fs::read(dir.join("w.txt")).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_opcode_removes_the_file() {
        use vfs_protocol::{encode_path_req, OP_DELETE, ST_OK};
        let dir = std::env::temp_dir().join(format!("vfs-rdd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("gone.txt"), b"x").unwrap();
        let d = Director::new();
        d.mount("/", std::sync::Arc::new(crate::DiskProvider::new(&dir))).unwrap();

        let (st, _) = dispatch_director(&d, OP_DELETE, &encode_path_req("gone.txt"), 0, 4096, None);
        assert_eq!(st, ST_OK);
        assert!(!dir.join("gone.txt").exists(), "OP_DELETE did not remove the file");
        let _ = std::fs::remove_dir_all(&dir);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-director --lib ring_dispatch`
Expected: FAIL — both opcodes fall through to the catch-all and return `ST_BAD_REQUEST`.

- [ ] **Step 3: Implement**

Add five match arms before the `_ =>` catch-all, each following the existing arms' shape: decode, call the `Director` method, record in `io_stats`, encode the response, map `Err(st)` to `(st, Vec::new())`, and a `None` decode to `(ST_BAD_REQUEST, Vec::new())`.

`OP_DELETE` uses `decode_path_req` (the same payload shape as `OP_GETATTR`).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p vfs-director`
Expected: all pass including the two new tests.

Run: `cargo clippy -p vfs-director --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-director/src/ring_dispatch.rs
git commit -m "feat(director): dispatch the five reserved write opcodes"
```

---

### Task 6: End-to-end write test

**Files:**
- Create: `crates/vfs-fixture-writepath/` (`Cargo.toml`, `src/main.rs`)
- Modify: `Cargo.toml` (workspace members), `crates/vfs-directord/tests/e2e.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: a fixture executable that, launched under a session, creates a file, writes, appends, renames, and deletes — with the host asserting the results landed in the provider graph.

- [ ] **Step 1: Write the fixture executable**

`crates/vfs-fixture-writepath/src/main.rs`: under its working directory, create `write-probe.txt` and write `hello`, reopen for append and append `world`, then exit 0. Every failure exits non-zero with a **distinct** code so the host can tell which step failed — `2` for create, `3` for write, `4` for append-open, `5` for append-write, `6` for readback mismatch. Keep it dependency-free — plain `std::fs`.

Do **not** include rename or delete yet: those need shim routing that Task 7 adds, and a fixture that fails for two possible reasons is a fixture that diagnoses neither.

- [ ] **Step 2: Write the failing host test**

In `crates/vfs-directord/tests/e2e.rs`, add a test that builds a session with a `DiskProvider` over a scratch directory, launches the fixture, waits for exit, and asserts the exit code is 0 and that `write-probe.txt` in the scratch directory contains `helloworld`. Assert on the **scratch directory contents**, not on a director query — the point is that the bytes landed where the provider graph says they should.

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p vfs-directord --test e2e`
Expected: FAIL — the fixture is not built or the writes do not land.

- [ ] **Step 4: Wire the fixture into the build**

Add the crate to the workspace `members` list and to the fixture-build list in `crates/vfs-directord/tests/e2e.rs` alongside `vfs-fixture-read`.

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test -p vfs-directord --test e2e`
Expected: PASS.

- [ ] **Step 6: Full gate**

```powershell
cargo build --all-targets
cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target
cargo test --workspace
cargo clippy --all-targets -- -D warnings
```

Expected: no failures; passing count above the 357 baseline.

- [ ] **Step 7: Commit**

```bash
git add rust/Cargo.toml rust/crates/vfs-fixture-writepath rust/crates/vfs-directord/tests/e2e.rs
git commit -m "test(e2e): a launched process writes through the director"
```

---

### Task 7: Shim — stop silently succeeding on delete and rename

**Files:**
- Modify: `crates/vfs-shim/src/hook.rs`, `crates/vfs-shim/src/fuse_client.rs`
- Modify: `crates/vfs-fixture-writepath/src/main.rs`, `crates/vfs-directord/tests/e2e.rs`

**Interfaces:**
- Consumes: the ring write opcodes (Task 5), the e2e fixture (Task 6).
- Produces: `FuseClient::delete(vpath)` and `FuseClient::rename(from, to)`; `NtSetInformationFile` on a synthetic handle routes `FileDispositionInformation` and `FileRenameInformation`.

**The bug being fixed:** `hook.rs:1792` returns `STATUS_SUCCESS` as a soft no-op for unhandled classes on synthetic handles. A game that deletes a virtual file is told it succeeded and nothing happens. Silent success is worse than failure.

**Why this is verified end-to-end rather than by a shim unit test:** the existing hook tests (`crates/vfs-shim/tests/hook_deny.rs`) drive the *snapshot* `Engine`, which phase 2a-ii deletes. Routing can only be observed against a live director and ring, so the fixture from Task 6 is the harness.

- [ ] **Step 1: Extend the fixture with rename and delete**

In `crates/vfs-fixture-writepath/src/main.rs`, after the append step, rename `write-probe.txt` to `write-probe-2.txt` (exit `7` on failure), then create and delete `write-probe-3.txt` (exit `8` on create failure, `9` on delete failure, `10` if it still exists afterwards).

- [ ] **Step 2: Extend the host test and run it to fail**

Extend the e2e test to assert that after exit: `write-probe-2.txt` contains `helloworld`, `write-probe.txt` does not exist, and `write-probe-3.txt` does not exist.

Run: `cargo test -p vfs-directord --test e2e`
Expected: FAIL. The delete reports success but `write-probe-3.txt` still exists — the silent no-op, observed.

- [ ] **Step 3: Add the client methods**

In `fuse_client.rs`, add `delete(&self, vpath: &str) -> Result<(), i32>` and `rename(&self, from: &str, to: &str) -> Result<(), i32>`, mirroring the existing `write` method's shape: take `ring_lock`, submit `OP_DELETE` with `encode_path_req` / `OP_RENAME` with `encode_rename_req`, and map a non-`ST_OK` status to `Err`.

There is **no flush opcode** in the catalog. `NtFlushBuffersFile` on a synthetic handle therefore stays a local success — record that as a known gap in your report rather than inventing an opcode, since opcode numbering is shared with the injected DLL.

- [ ] **Step 4: Route the two classes**

In the `NtSetInformationFile` hook, before the soft no-op, match `FileDispositionInformation` and `FileRenameInformation` on synthetic handles and route them through the client. Keep the soft no-op for other classes, but **log every class that takes it** via `hookstats` so the set is discoverable rather than assumed empty — that assumption is what produced this bug.

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test -p vfs-directord --test e2e`
Expected: PASS.

Run: `cargo clippy -p vfs-shim -p vfs-directord --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Full gate**

```powershell
cargo build --all-targets
cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target
cargo test --workspace
cargo clippy --all-targets -- -D warnings
```

Expected: no failures; passing count above the 357 baseline.

- [ ] **Step 7: Commit**

```bash
git add rust/crates/vfs-shim rust/crates/vfs-fixture-writepath rust/crates/vfs-directord/tests/e2e.rs
git commit -m "fix(shim): route delete and rename instead of silently succeeding"
```

---

## Phase 2a-i Exit Criteria

- [ ] `assert_conformance` runs write cases for any `ReadWrite` provider, with a negative fixture proving they bite.
- [ ] `DiskProvider` and `OverlayProvider` both declare `ReadWrite` and pass full conformance.
- [ ] `Director::open` accepts `OPEN_WRITE`; append is resolved by the director, not by providers.
- [ ] A write to a read-only stack returns `ST_READ_ONLY` and is recorded for discovery.
- [ ] All five reserved write opcodes dispatch.
- [ ] The shim routes delete and rename rather than silently succeeding.
- [ ] A launched process writes, appends, renames, and deletes through the director end to end.
- [ ] Full gate green; passing count above 357.

**Explicitly NOT in this phase:** deleting `Decision::Redirect` / `Serve`, closing the DRM exceptions, making the root fully virtual, path canonicalisation, the escape canary suite, and the open-count reconciliation. All of that is phase 2a-ii, and mixing it in would make a failure impossible to attribute.
