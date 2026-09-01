# Case-Fold Contract Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** make case-insensitive resolution a declared, conformance-tested property of a provider, and make the in-tree providers that should be case-insensitive actually be so — closing spec §6b.

**Architecture:** `Capabilities` gains a `CaseMatch` declaration that `weakest()` combines across a composed graph. `assert_conformance` holds every provider to its declaration. `MemoryProvider` and `InlineProvider` gain a folded index keyed the way `vfs-zip`'s `by_fold` already works — exact-match first, folded lookup on a miss, original spelling preserved for `readdir`. `SubdirProvider` folds its prefix mapping. `DiskProvider` folds itself on non-Windows, where the OS does not.

**Three providers, not four.** The spec names `memory`, `inline`, `router` and `subdir` as non-folding, which is true of the source but not of the behaviour. `RouterProvider` needs **no change**: it selects routes through `glob::matches` (`router.rs:50`) and `glob.rs` folds, and its `capabilities()` already delegates to `Capabilities::weakest(...)` over default + routes (`router.rs:59-64`), so Task 1's `weakest` change makes its declaration correct with no edit. `SubdirProvider` is the opposite and worse: `capabilities()` returns `self.inner.capabilities()` unchanged (`subdir.rs:39-41`), so it will *inherit* an `Insensitive` claim from its child while its own `map_path` prefix work stays byte-exact — a claim it does not honour.

**Tech Stack:** Rust 2021, cargo workspace at `rust/`. Verification is the Windows suite plus `cargo check --target x86_64-unknown-linux-gnu`; `DiskProvider`'s non-Windows behaviour is verified by the `rust-linux-portable` CI job.

**Spec:** `docs/superpowers/specs/2026-08-31-case-fold-contract-design.md`

## Global Constraints

- **The wire does not change.** The shim keeps folding before send. Do not touch `vfs-redirect`'s `match_canonical`, the ring protocol, or the overlay's on-disk layout. `bin/regen-protocol` must produce no diff under `resources/`.
- **Windows behaviour must not change** other than the intended case-insensitive resolution in the four named providers.
- **`vfs-provider` keeps zero dependencies.** `CaseMatch` is a plain enum needing only `std`.
- **`cargo clippy --all-targets -- -D warnings` must stay clean, without suppressions.** `#[allow(...)]` resolutions are ruled out — that ruling was made twice on the previous increment.
- **`vfs_core::fold` is the only definition of fold-equality.** Never `to_ascii_lowercase`. That exact substitution has already shipped a bug here: `Data/ÜBER/a.esp` crossed the ring folded while every index below was keyed unfolded.
- **The fold is not length-preserving.** `İ` (U+0130) is two bytes and folds to three. Never slice a folded string by an offset measured on the unfolded one — walk components. `strip_prefix` and `mount_child_name` both broke this way before.
- **No test may be edited to make a suite green.** The one sanctioned test change is converting `primitives.test.mts`'s §6b `test.fails` to an ordinary test in Task 7 — and it must be converted, never deleted, because it is the only evidence the hole existed.
- **Every task must leave the whole suite green.** `assert_conformance` is called from 12 test sites (`vfs-cache/src/provider.rs`, `vfs-compose/src/{layered,lib,memory,overlay,readonly,router}.rs`), so a declaration that outruns its implementation breaks all of them.
- All `cargo` commands run from `rust/`.

---

## File Structure

**Modified:**
- `rust/crates/vfs-provider/src/caps.rs` — `CaseMatch` enum, `Capabilities::case`, combination in `weakest()`
- `rust/crates/vfs-provider/src/lib.rs` — export `CaseMatch`
- `rust/crates/vfs-provider/src/conformance.rs` — `assert_case`, wired into `assert_conformance`
- `rust/crates/vfs-provider/src/path.rs` — correct `VPath`'s doc comment
- `rust/crates/vfs-compose/src/memory.rs` — folded index; declaration flips to `Insensitive`
- `rust/crates/vfs-compose/src/inline.rs` — same
- `rust/crates/vfs-compose/src/subdir.rs` — fold the prefix mapping so its inherited claim is honoured; gains an `assert_conformance` test, which it has none of today

**Not modified: `rust/crates/vfs-compose/src/router.rs`.** It routes through `glob::matches`, which folds, and derives its capabilities with `Capabilities::weakest(...)`. Task 1 makes it correct without touching it. If you find yourself editing it, stop and re-read `router.rs:50` and `:59-64` — the plan expects zero diff there.
- `rust/crates/vfs-director/src/disk.rs` — fold-scan on non-Windows
- `rust/crates/vfs-node/test/primitives.test.mts` — §6b `test.fails` becomes a passing test

**No new files.** Every change lands in the module that owns the behaviour, which is what keeps the fold definition in one place rather than spreading a helper across crates.

---

### Task 1: `CaseMatch`, and honest declarations

The declaration lands first, with the four known-sensitive providers declaring `Sensitive` **truthfully**, before any conformance case exists to check them. Doing it the other way round — default everything to `Insensitive` and add checks after — breaks all 12 `assert_conformance` sites at once.

**Files:**
- Modify: `rust/crates/vfs-provider/src/caps.rs`
- Modify: `rust/crates/vfs-provider/src/lib.rs`
- Modify: `rust/crates/vfs-compose/src/{memory,inline,router,subdir}.rs` (declaration only)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub enum CaseMatch { Insensitive, Sensitive }` — `Clone, Copy, Debug, PartialEq, Eq`
  - `Capabilities { .., pub case: CaseMatch }`
  - `Capabilities::read_only()` sets `case: CaseMatch::Insensitive`
  - `Capabilities::weakest()` yields `Insensitive` only when **every** child is `Insensitive`

- [ ] **Step 1: Write the failing tests**

Append to `caps.rs`'s existing `mod tests`:

```rust
    #[test]
    fn read_only_declares_case_insensitive_because_that_is_what_windows_needs() {
        assert_eq!(Capabilities::read_only().case, CaseMatch::Insensitive);
    }

    /// A graph is only as case-insensitive as its least-insensitive leaf. One
    /// `Sensitive` child makes the whole composition `Sensitive`, the same way
    /// one non-immutable child makes it mutable.
    #[test]
    fn weakest_is_case_sensitive_if_any_child_is() {
        let ins = Capabilities::read_only();
        let sen = Capabilities { case: CaseMatch::Sensitive, ..Capabilities::read_only() };
        assert_eq!(Capabilities::weakest([ins, sen]).case, CaseMatch::Sensitive);
        assert_eq!(Capabilities::weakest([sen, ins]).case, CaseMatch::Sensitive);
    }

    #[test]
    fn weakest_stays_insensitive_when_all_children_are() {
        let ins = Capabilities::read_only();
        assert_eq!(Capabilities::weakest([ins, ins]).case, CaseMatch::Insensitive);
    }

    /// The combinators that pass access through must not silently reset case.
    #[test]
    fn the_passthrough_combinators_preserve_the_case_declaration() {
        let sen = Capabilities { case: CaseMatch::Sensitive, ..Capabilities::read_only() };
        assert_eq!(sen.seekable().case, CaseMatch::Sensitive);
        assert_eq!(sen.cached().case, CaseMatch::Sensitive);
        assert_eq!(sen.read_only_clamp().case, CaseMatch::Sensitive);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vfs-provider --lib caps`
Expected: FAIL — `no field 'case' on type 'Capabilities'` / `cannot find type 'CaseMatch'`.

- [ ] **Step 3: Add the enum and the field**

In `caps.rs`, above `Capabilities`:

```rust
/// How this provider matches a name it is given.
///
/// Declared, not probed — like every other capability here. The composition
/// layer reads it to select conformance cases, and a future FUSE mount will
/// read it to refuse a `Sensitive` provider outright, since a Windows program
/// over one is broken by construction.
///
/// This exists because two delivery paths disagreed about the spelling a
/// provider receives: the shim folds a vpath before sending it
/// (`vfs-redirect`'s `match_canonical`), while a host-side caller
/// (`vfs-embed`, `vfs-node`, this crate's conformance suite) sends the
/// original case. A provider that resolves fold-equal names identically is
/// correct under both, which is why the guarantee lives here rather than at
/// either boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseMatch {
    /// Fold-equal names resolve to the same entry, where fold-equal means
    /// [`vfs_core::fold`] — not `to_ascii_lowercase`, and not "the OS will
    /// sort it out".
    Insensitive,
    /// Byte-exact names only. Correct for a provider over a case-sensitive
    /// store that has not indexed for folding; **not** safe under a FUSE mount
    /// serving a Windows program.
    Sensitive,
}
```

Add `pub case: CaseMatch,` to `Capabilities` with this doc line:

```rust
    /// How names are matched. See [`CaseMatch`]; `Insensitive` is what a
    /// Windows-facing VFS must provide.
    pub case: CaseMatch,
```

Set it in `read_only()`:

```rust
        Capabilities {
            access: Access::Read,
            immutable: false,
            slow: false,
            preferred_block: None,
            case: CaseMatch::Insensitive,
        }
```

In `weakest()`, add to the `Some(acc)` arm alongside the existing fields:

```rust
                    case: match (acc.case, c.case) {
                        (CaseMatch::Insensitive, CaseMatch::Insensitive) => CaseMatch::Insensitive,
                        _ => CaseMatch::Sensitive,
                    },
```

`seekable`, `cached` and `read_only_clamp` all use `..self`, so they preserve `case` with no edit — the Step 1 test pins that.

**Do not add a `validate()` rule for `case`.** There is no contradiction to catch: any access level combines with either case value. A rule invented here would be a guess.

- [ ] **Step 4: Export it**

In `rust/crates/vfs-provider/src/lib.rs`, add `CaseMatch` to the existing `pub use caps::{...}` list.

- [ ] **Step 5: Declare the two genuinely-sensitive providers honestly**

`MemoryProvider` and `InlineProvider` own their own storage and match byte-exactly, so `Insensitive` would be a false claim. In each, find the `capabilities()` implementation and add the field explicitly:

```rust
        // Sensitive until this provider folds — Tasks 3 and 4 of the case-fold plan.
        Capabilities { case: CaseMatch::Sensitive, ..Capabilities::read_only() }
```

- `rust/crates/vfs-compose/src/memory.rs`
- `rust/crates/vfs-compose/src/inline.rs`

**Change nothing else.** Two providers the spec also named turn out not to need a declaration:

- `RouterProvider` builds its capabilities with `Capabilities::weakest(...)` over default + routes (`router.rs:59-64`), so `case` now derives from its children — the correct answer, and an explicit declaration here would *override* it wrongly.
- `SubdirProvider` returns `self.inner.capabilities()` (`subdir.rs:39-41`), so it likewise inherits. That inheritance is right in principle and currently unearned — its `map_path` is byte-exact — which is Task 5's job. Declaring it `Sensitive` here would be wrong in the other direction: it would make a subdir over a *sensitive* child claim something its child already says.

- [ ] **Step 5a: Confirm you changed exactly two files in vfs-compose**

Run: `git diff --stat rust/crates/vfs-compose`
Expected: `memory.rs` and `inline.rs` only. A diff in `router.rs` means you added a declaration that `weakest` should be deriving.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p vfs-provider -p vfs-compose --no-fail-fast`
Expected: PASS. No conformance case reads `case` yet, so nothing else moves.

- [ ] **Step 7: Verify both platforms and clippy**

```bash
cargo clippy --all-targets -- -D warnings
cargo check --target x86_64-unknown-linux-gnu -p vfs-provider -p vfs-compose
```
Expected: both clean.

- [ ] **Step 8: Commit**

```bash
git add rust/crates/vfs-provider rust/crates/vfs-compose
git commit -m "feat(provider): declare how a provider matches case

Capabilities::case, combined across a graph by weakest() — one Sensitive leaf
makes the whole composition Sensitive. The four providers that do not fold
declare Sensitive truthfully, before any conformance case exists to check them."
```

---

### Task 2: Hold providers to the declaration

**Files:**
- Modify: `rust/crates/vfs-provider/src/conformance.rs`

**Interfaces:**
- Consumes: `CaseMatch` and `Capabilities::case` from Task 1.
- Produces: `assert_case` runs inside `assert_conformance` for every provider.

- [ ] **Step 1: Write the case suite**

Add to `conformance.rs`. `FIXTURE_FILES` and `write_fixture_tree` already exist in this module and are what `assert_common` uses.

```rust
/// Hold a provider to its declared [`CaseMatch`].
///
/// Both directions are checked. A provider claiming `Insensitive` while
/// matching byte-exactly is the §6b defect. A provider claiming `Sensitive`
/// while folding is also a broken contract — a composition may have been built
/// on the strictness it advertised.
///
/// Coverage is split by access level, and deliberately:
///
/// - **Every provider** gets the ASCII check below, against `FIXTURE_FILES`.
/// - **Writable providers** additionally get a non-ASCII check, because it can
///   seed its own file. `FIXTURE_FILES` is ASCII-only (`a.txt`, `sub/b.txt`),
///   so a read-only provider cannot be held to Unicode folding here — that
///   coverage lives in each provider's own tests instead.
///
/// The non-ASCII case is not decoration: `to_ascii_lowercase` passes an
/// ASCII-only suite, and this project shipped exactly that bug. `Data/ÜBER/a.esp`
/// crossed the ring folded while every index below was keyed unfolded, so the
/// file resolved to not-found — and `DiskProvider` hid it, because Windows folds
/// Unicode itself.
fn assert_case(p: &Arc<dyn Provider>, case: CaseMatch) {
    // A file the fixture tree is known to contain, in its seeded spelling.
    let (seeded, body) = FIXTURE_FILES[0];
    let upper = seeded.to_uppercase();
    if upper == seeded {
        panic!(
            "conformance: FIXTURE_FILES[0] ({seeded}) has no case variant, so the \
             case cases cannot distinguish folded from exact matching"
        );
    }

    let found_upper = p
        .getattr(VPath::at_default(&upper))
        .expect("getattr must not error on a differently-cased name, only report absence");

    match case {
        CaseMatch::Insensitive => {
            let st = found_upper.unwrap_or_else(|| {
                panic!(
                    "declares CaseMatch::Insensitive but did not resolve {upper}, \
                     the fold-equal spelling of the seeded {seeded}"
                )
            });
            assert_eq!(
                st.size,
                body.len() as u64,
                "resolved {upper} to an entry of the wrong size — it matched \
                 something other than {seeded}"
            );

            // A handle opened through the alternate spelling must read the same
            // bytes. `getattr` agreeing is not enough: the open path may key
            // differently from the stat path.
            let (h, _len, _) = p
                .open(VPath::at_default(&upper), OPEN_READ)
                .expect("open must accept a fold-equal spelling when Insensitive");
            let mut buf = vec![0u8; body.len()];
            let n = p.read_at(h, 0, &mut buf).expect("read_at through the alternate spelling");
            p.close(h).expect("close");
            assert_eq!(&buf[..n], body, "the alternate spelling read different bytes");
        }
        CaseMatch::Sensitive => {
            assert!(
                found_upper.is_none(),
                "declares CaseMatch::Sensitive but resolved {upper}, the fold-equal \
                 spelling of {seeded} — a composition may rely on that strictness"
            );
        }
    }

    // Non-ASCII, for providers that can seed their own file. `vfs_core::fold`
    // is Unicode-aware; `to_ascii_lowercase` is not, and substituting one for
    // the other passes every ASCII case above.
    if p.capabilities().access == Access::ReadWrite {
        let (h, _len, _) = p
            .open(VPath::at_default("Über.txt"), OPEN_WRITE)
            .expect("open for write to seed the non-ASCII case");
        p.write_at(h, 0, b"x").expect("seed write");
        p.close(h).expect("close the seeded file");

        let lower = p
            .getattr(VPath::at_default("über.txt"))
            .expect("getattr must not error on a differently-cased non-ASCII name");
        match case {
            CaseMatch::Insensitive => assert!(
                lower.is_some(),
                "declares CaseMatch::Insensitive but did not resolve über.txt after \
                 seeding Über.txt — a Unicode-unaware fold (to_ascii_lowercase) \
                 passes every ASCII case and fails exactly here"
            ),
            CaseMatch::Sensitive => assert!(
                lower.is_none(),
                "declares CaseMatch::Sensitive but resolved über.txt after seeding \
                 Über.txt"
            ),
        }

        p.remove(VPath::at_default("Über.txt")).expect("clean up the seeded file");
    }
}
```

`OPEN_WRITE`, `Access` and `CaseMatch` must all be reachable in this module. `conformance.rs:13` imports from `crate::{...}` — add whichever of these are missing to that list. `OPEN_READ` is already there (`assert_positional` uses it).

The seeded file is removed at the end so `assert_case` leaves the provider as it found it — `assert_conformance` runs `assert_writable` after this, and that suite has its own expectations about what the tree contains.

- [ ] **Step 2: Wire it in**

In `assert_conformance`, after the `assert_common(&p);` line:

```rust
    assert_case(&p, caps.case);
```

Place it before the access-level dispatch: it uses only `getattr`/`open`/`read_at`, which every access level provides, and a case failure is more informative than the positional-read failure it would otherwise cause downstream.

- [ ] **Step 3: Run the suite**

Run: `cargo test -p vfs-provider -p vfs-compose -p vfs-cache --no-fail-fast`
Expected: PASS. Task 1 made every declaration honest, so both branches hold.

If a provider fails here, its Task 1 declaration was wrong — fix the declaration, not the test. That is a real finding worth reporting: it means a provider folds or fails to fold where nobody thought it did.

- [ ] **Step 4: Prove the check can actually fail**

A conformance case that cannot fail is decoration. Verify by hand, without committing:

```bash
# Temporarily flip memory.rs's declaration to Insensitive (it does not fold yet)
cargo test -p vfs-compose --lib memory
```
Expected: FAIL with "declares CaseMatch::Insensitive but did not resolve …".
Then revert the flip. Record the observed failure text in your report — this is the evidence that Task 3 is doing something real.

- [ ] **Step 5: Clippy and Linux check**

```bash
cargo clippy --all-targets -- -D warnings
cargo check --target x86_64-unknown-linux-gnu -p vfs-provider
```

- [ ] **Step 6: Commit**

```bash
git add rust/crates/vfs-provider/src/conformance.rs
git commit -m "test(provider): hold every provider to its declared CaseMatch

Both directions: Insensitive must resolve a fold-equal spelling through both
getattr and open; Sensitive must refuse it. Includes a non-ASCII case, because
to_ascii_lowercase passes an ASCII-only suite and that substitution has already
shipped a bug here."
```

---

### Task 3: `MemoryProvider` folds

**Files:**
- Modify: `rust/crates/vfs-compose/src/memory.rs`

**Interfaces:**
- Consumes: `CaseMatch` (Task 1), `assert_case` (Task 2).
- Produces: `MemoryProvider` declares `CaseMatch::Insensitive`.

The pattern is `vfs-zip`'s, which is the proven one in this codebase (`vfs-zip/src/backend.rs:45,93-96,110-114`): a `HashMap<folded, original>` beside the real map, consulted **only on an exact miss**, so the common path pays no fold allocation and `readdir` keeps returning original spellings.

- [ ] **Step 1: Write the failing test**

Add to `memory.rs`'s test module:

```rust
    /// Fold-equal spellings name the same entry, and the seeded spelling is
    /// what `readdir` reports — folding is a lookup property, not a storage
    /// one. Writing through a variant spelling must hit the same file rather
    /// than creating a sibling: that sibling is spec §6b.
    #[test]
    fn fold_equal_spellings_resolve_to_one_entry() {
        let p = MemoryProvider::from_files([("Data/A.esp", &b"body"[..])]);

        for spelling in ["Data/A.esp", "data/a.esp", "DATA/A.ESP", "dAtA/a.EsP"] {
            let st = p
                .getattr(VPath::at_default(spelling))
                .unwrap()
                .unwrap_or_else(|| panic!("{spelling} did not resolve"));
            assert_eq!(st.size, 4, "{spelling} resolved to the wrong entry");
        }

        let names: Vec<String> = p
            .readdir(VPath::at_default("Data"))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["A.esp".to_string()], "readdir must report the seeded spelling");
    }

    /// Non-ASCII, because `to_ascii_lowercase` would pass every case above.
    #[test]
    fn folding_is_unicode_not_ascii() {
        let p = MemoryProvider::from_files([("Über/A.esp", &b"x"[..])]);
        assert!(
            p.getattr(VPath::at_default("über/a.esp")).unwrap().is_some(),
            "Unicode fold-equal spelling did not resolve"
        );
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vfs-compose --lib memory::tests::fold_equal_spellings_resolve_to_one_entry`
Expected: FAIL — `data/a.esp did not resolve`.

- [ ] **Step 3: Add the folded index**

`MemoryProvider` is mutable (files are inserted and removed at runtime), so unlike `ZipProvider`'s immutable map the index must be maintained. Keep it inside the existing `Mutex` discipline rather than adding a second lock — add a field:

```rust
    /// Folded key → the spelling `files`/`dirs` is actually keyed by. Consulted
    /// only when an exact lookup misses, so the common path pays no fold.
    /// Maintained alongside every mutation of `files` and `dirs`; a stale entry
    /// here resolves a name to a file that no longer exists.
    by_fold: Mutex<HashMap<String, String>>,
```

Add a private resolver:

```rust
    /// The stored spelling for `path`, or `None` if nothing fold-equal exists.
    /// Exact match first — that is the hot path and needs no allocation.
    fn canonical(&self, path: &str) -> Option<String> {
        let files = self.files.lock().unwrap();
        let dirs = self.dirs.lock().unwrap();
        if files.contains_key(path) || dirs.contains(path) {
            return Some(path.to_string());
        }
        drop(files);
        drop(dirs);
        self.by_fold.lock().unwrap().get(&vfs_core::fold(path)).cloned()
    }
```

Then route every place that looks a path up through `canonical` — `getattr`, `open`, `read_at`'s open bookkeeping, `write_at`, `remove`, `rename`, `set_len`, `readdir`'s directory resolution — and every place that inserts or removes a key must update `by_fold` in the same critical section.

Walk the file and handle each site; do not pattern-match on the list above and assume it is complete. When you are done, `grep -n "files.lock()\|dirs.lock()" src/memory.rs` and confirm every hit either goes through `canonical` or is a mutation that also maintains `by_fold`. Report the list you found in your report.

**Directory paths fold too.** `getattr("DATA")` must find the directory implied by `Data/A.esp`. `child_prefix` builds `"path/"` for the children scan, so it must be built from the canonical spelling, not the caller's.

`vfs-compose` already depends on `vfs-core` (it is in the fold users list), so no manifest change is needed. Verify with `grep -n vfs-core crates/vfs-compose/Cargo.toml` before assuming.

- [ ] **Step 4: Flip the declaration**

Replace Task 1's honest `Sensitive` with the default, and delete the `// Sensitive until this provider folds` comment:

```rust
        Capabilities::read_only()
```

(or keep whatever other fields it set, dropping only `case`.)

- [ ] **Step 5: Run the tests**

Run: `cargo test -p vfs-compose --no-fail-fast`
Expected: PASS, including the six `assert_conformance` sites in this crate, which now run the `Insensitive` branch for anything built over `MemoryProvider`.

Run: `cargo test -p vfs-cache --no-fail-fast`
Expected: PASS. `CachingProvider` wraps memory in its conformance tests and inherits `Insensitive` through `weakest`. If it fails, the cache keys blocks by the caller's spelling rather than a canonical one — report it; do not fix it here.

- [ ] **Step 6: Clippy and Linux check**

```bash
cargo clippy --all-targets -- -D warnings
cargo check --target x86_64-unknown-linux-gnu -p vfs-compose
```

- [ ] **Step 7: Commit**

```bash
git add rust/crates/vfs-compose/src/memory.rs
git commit -m "fix(compose): MemoryProvider resolves fold-equal names

Closes the provider half of spec §6b. Folded index beside the real map,
consulted only on an exact miss, so readdir keeps the seeded spelling and the
hot path pays no fold — the same shape as vfs-zip's by_fold."
```

---

### Task 4: `InlineProvider` folds

**Files:**
- Modify: `rust/crates/vfs-compose/src/inline.rs`

**Interfaces:**
- Consumes: Task 1 and 2.
- Produces: `InlineProvider` declares `CaseMatch::Insensitive`.

`inline.rs` has **zero** occurrences of `fold` today. Unlike `MemoryProvider` it is immutable after construction — `files: HashMap<String, FileData>` with no `Mutex` around it (`inline.rs:17-21`) — so the `ZipProvider` shape applies directly: build the index once in `from_files`, no maintenance, no locking.

- [ ] **Step 1: Write the failing test**

Add to `inline.rs`'s test module. `InlineProvider::from_files` takes `IntoIterator<Item = (P: AsRef<str>, B: AsRef<[u8]>)>`:

```rust
    /// Fold-equal spellings name the same entry. `InlineProvider` is the leaf
    /// under most composed test stacks, so a byte-exact match here makes every
    /// stack above it byte-exact too.
    #[test]
    fn fold_equal_spellings_resolve_to_one_entry() {
        let p = InlineProvider::from_files([("Data/A.esp", &b"body"[..])]);

        for spelling in ["Data/A.esp", "data/a.esp", "DATA/A.ESP", "dAtA/a.EsP"] {
            let st = p
                .getattr(VPath::at_default(spelling))
                .unwrap()
                .unwrap_or_else(|| panic!("{spelling} did not resolve"));
            assert_eq!(st.size, 4, "{spelling} resolved to the wrong entry");
        }
    }

    /// Non-ASCII, because `to_ascii_lowercase` would pass every case above.
    #[test]
    fn folding_is_unicode_not_ascii() {
        let p = InlineProvider::from_files([("Über/A.esp", &b"x"[..])]);
        assert!(
            p.getattr(VPath::at_default("über/a.esp")).unwrap().is_some(),
            "Unicode fold-equal spelling did not resolve"
        );
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vfs-compose --lib inline::tests::fold_equal_spellings_resolve_to_one_entry`
Expected: FAIL — `data/a.esp did not resolve`.

- [ ] **Step 3: Implement**

Add the field to `InlineProvider`:

```rust
    /// Folded key → the spelling `files` is keyed by. Built once; this provider
    /// is immutable after construction, so unlike `MemoryProvider`'s index this
    /// one needs no maintenance and no lock.
    by_fold: HashMap<String, String>,
```

Build it in `from_files` after the `files` map is populated, mirroring `vfs-zip/src/backend.rs:93-96`:

```rust
        let mut by_fold = HashMap::with_capacity(files.len());
        for key in files.keys() {
            by_fold.insert(vfs_core::fold(key), key.clone());
        }
```

and add it to the returned struct literal. Then add the resolver, mirroring `backend.rs:110-114` — exact first, so a hit costs no fold:

```rust
    /// The stored spelling for `path`, or `None` if nothing fold-equal exists.
    fn canonical(&self, path: &str) -> Option<&String> {
        if self.files.contains_key(path) {
            return self.files.get_key_value(path).map(|(k, _)| k);
        }
        self.by_fold.get(&vfs_core::fold(path))
    }
```

Route every `files.get(...)` / `files.contains_key(...)` lookup through it — `getattr`, `open`, `readdir`'s directory resolution, and anything else in the file. When done, `grep -n "self.files" src/inline.rs` and confirm every read goes through `canonical`. Report the list you found.

Confirm `vfs-core` is already a dependency: `grep -n vfs-core crates/vfs-compose/Cargo.toml`.

- [ ] **Step 4: Flip the declaration to the `read_only()` default and delete the placeholder comment.**

- [ ] **Step 5: Run the tests**

Run: `cargo test -p vfs-compose --no-fail-fast`
Expected: PASS.

- [ ] **Step 6: Clippy, then commit**

```bash
cargo clippy --all-targets -- -D warnings
git add rust/crates/vfs-compose/src/inline.rs
git commit -m "fix(compose): InlineProvider resolves fold-equal names"
```

---

### Task 5: `SubdirProvider` honours the claim it inherits

`SubdirProvider::capabilities()` returns `self.inner.capabilities()` verbatim (`subdir.rs:39-41`). So over an `Insensitive` child it advertises `Insensitive` — while its own `map_path` joins and strips the prefix byte-exactly. It promises what its child provides and then breaks the promise on the way through.

It also has **no `assert_conformance` test today** (the twelve call sites are in `vfs-cache/src/provider.rs` and `vfs-compose/src/{layered,lib,memory,overlay,readonly,router}.rs`), which is why this went unnoticed. This task adds one.

**Files:**
- Modify: `rust/crates/vfs-compose/src/subdir.rs`

**Interfaces:**
- Consumes: `CaseMatch` (Task 1), `assert_case` (Task 2), a folding `InlineProvider` (Task 4) to use as the inner provider in tests.
- Produces: `SubdirProvider` honours an inherited `Insensitive` claim. Its `capabilities()` stays a pass-through — do not add a declaration.

**The hazard:** the fold is **not length-preserving**. `İ` (U+0130) is two bytes and folds to three. So you may not fold a path, measure the prefix length in the folded string, and slice the *original* at that offset — `strip_prefix` and `mount_child_name` both did exactly that and broke. Compare component by component and rebuild the remainder from the original string's components, counting components rather than bytes. `vfs-compose/src/glob.rs` folds correctly and is the in-crate reference.

- [ ] **Step 1: Write the failing tests**

Add to `subdir.rs`'s test module. `SubdirProvider::new(inner: Arc<dyn Provider>, prefix: impl Into<String>)` and `InlineProvider::from_files(entries)` are the real constructors:

```rust
    /// Stripping an archive root must strip fold-equally: the root's spelling
    /// comes from the zip's own entry names, the request's spelling comes from
    /// the game. This provider inherits its child's `Insensitive` claim, so it
    /// must honour it rather than pass it on unearned.
    #[test]
    fn the_prefix_is_matched_fold_equally() {
        let inner: Arc<dyn Provider> =
            Arc::new(InlineProvider::from_files([("Root/Data/A.esp", &b"body"[..])]));
        let s = SubdirProvider::new(inner, "Root");

        for spelling in ["Data/A.esp", "data/a.esp", "DATA/A.ESP"] {
            assert!(
                s.getattr(VPath::at_default(spelling)).unwrap().is_some(),
                "{spelling} did not resolve through the stripped prefix"
            );
        }
    }

    /// The fold is not length-preserving: `Ü` folds to a different byte length.
    /// A prefix containing one must still strip correctly, which it will not if
    /// the remainder is sliced at an offset measured on the folded string.
    #[test]
    fn a_prefix_whose_fold_changes_byte_length_still_strips() {
        let inner: Arc<dyn Provider> =
            Arc::new(InlineProvider::from_files([("Über/a.esp", &b"x"[..])]));
        let s = SubdirProvider::new(inner, "Über");
        assert!(s.getattr(VPath::at_default("a.esp")).unwrap().is_some());
        assert!(s.getattr(VPath::at_default("A.ESP")).unwrap().is_some());
    }

    /// The systematic guard: this module had no conformance test at all, which
    /// is how an unearned capability claim survived here.
    #[test]
    fn a_subdir_over_the_fixture_tree_passes_conformance() {
        let inner: Arc<dyn Provider> = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES
                .iter()
                .map(|(rel, body)| (format!("Root/{rel}"), *body)),
        ));
        let s: Arc<dyn Provider> = Arc::new(SubdirProvider::new(inner, "Root"));
        vfs_provider::assert_conformance(s);
    }
```

Add whatever `use` lines the module's test block lacks (`std::sync::Arc`, `crate::InlineProvider`, `vfs_provider::{Provider, VPath}`) — follow `router.rs`'s test module, which does the same thing.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p vfs-compose --lib subdir`
Expected: FAIL — `data/a.esp did not resolve through the stripped prefix`, and the conformance test failing its `Insensitive` branch.

- [ ] **Step 3: Fold `map_path`, component-wise**

Make the prefix comparison in `map_path` (and any sibling that strips the prefix on the way back out, e.g. for `readdir` names) compare folded components, then rebuild from the original components. Do not introduce byte offsets computed on folded strings.

- [ ] **Step 4: Leave `capabilities()` alone**

It stays `self.inner.capabilities()`. The claim was always right; only the implementation was missing. Adding a declaration here would break a subdir over a `Sensitive` child.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p vfs-compose -p vfs-cache --no-fail-fast`
Expected: PASS.

- [ ] **Step 6: Clippy, then commit**

```bash
cargo clippy --all-targets -- -D warnings
git add rust/crates/vfs-compose/src/subdir.rs
git commit -m "fix(compose): SubdirProvider honours the case claim it inherits

capabilities() passes the child's declaration straight through, so over an
Insensitive child it advertised Insensitive while map_path stripped the prefix
byte-exactly. Folds component-wise, because the fold is not length-preserving —
the mistake that broke strip_prefix and mount_child_name before. Adds the
conformance test this module never had, which is how it went unnoticed."
```

---

### Task 6: `DiskProvider` folds where the OS does not

**Files:**
- Modify: `rust/crates/vfs-director/src/disk.rs`

**Interfaces:**
- Consumes: Task 1 and 2.
- Produces: `DiskProvider` resolves fold-equally on **all** targets, not only where NTFS does it for free.

On Windows, NTFS matches case-insensitively, so `DiskProvider` is already `Insensitive` without trying — which is why the gap was invisible. On Linux over ext4 it is byte-exact. Declaring that honestly per-platform would be truthful and useless: it would make `DiskProvider` unusable under the FUSE mount this whole arc exists to build.

- [ ] **Step 1: Write the failing test**

Add to `disk.rs`'s test module. It must be meaningful on both platforms — passing trivially on Windows is expected, and the Linux CI job is what makes it bite:

```rust
    /// Fold-equal resolution must not depend on the host filesystem. On Windows
    /// NTFS satisfies this for free; on Linux over ext4 nothing does, and a
    /// FUSE mount serving a Windows program needs it either way.
    #[test]
    fn fold_equal_spellings_resolve_on_any_filesystem() {
        let dir = std::env::temp_dir().join(format!("vfs-disk-fold-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("Data"));
        std::fs::write(dir.join("Data").join("A.esp"), b"body").unwrap();

        let p = DiskProvider::new(&dir);
        for spelling in ["Data/A.esp", "data/a.esp", "DATA/A.ESP"] {
            let st = p
                .getattr(VPath::at_default(spelling))
                .unwrap()
                .unwrap_or_else(|| panic!("{spelling} did not resolve"));
            assert_eq!(st.size, 4, "{spelling} resolved to the wrong entry");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test -p vfs-director --lib disk`
Expected on Windows: **PASS** — NTFS folds already. That is not a failed RED step; it is the platform difference the test exists to pin. Record that it passed before the change, so the Linux result is the meaningful one.

- [ ] **Step 3: Implement the fold-scan for non-Windows**

Guard it so Windows keeps using the OS and pays nothing:

```rust
/// Resolve `rel` against `base` when the host filesystem is case-sensitive.
///
/// Exact path first — the hit costs one syscall and no allocation. On a miss,
/// walk components, and for each one that does not exist byte-exactly, scan the
/// containing directory for a fold-equal entry. This is what Wine does, and
/// what `ciopfs` exists to avoid doing repeatedly.
///
/// Compares with [`vfs_core::fold`], never `to_ascii_lowercase`, and never
/// hands a folded spelling to the filesystem: `casefold.rs` warns the fold is
/// not NTFS-case-equivalence (`İ` folds to a genuinely different name), so the
/// resolved *original* entry name is what gets opened.
#[cfg(not(windows))]
fn resolve_fold_equal(base: &std::path::Path, rel: &str) -> Option<std::path::PathBuf> {
    // implement per the doc above
}
```

Route `DiskProvider`'s path construction through it on `cfg(not(windows))`, leaving the Windows path byte-identical.

**Cache nothing.** The spec says measure first. A miss costs a `readdir` of one directory, only where the OS does not fold.

- [ ] **Step 4: Run on both targets**

```bash
cargo test -p vfs-director --no-fail-fast
cargo check --target x86_64-unknown-linux-gnu -p vfs-director --all-targets
```
Expected: Windows PASS; Linux check clean. The Linux *run* happens in CI — note in your report that local verification cannot cover it, because this machine has no cross-linker.

- [ ] **Step 5: Clippy, then commit**

```bash
cargo clippy --all-targets -- -D warnings
git add rust/crates/vfs-director/src/disk.rs
git commit -m "fix(director): DiskProvider folds where the filesystem does not

NTFS made this free on Windows and invisible everywhere else. A FUSE mount
serving a Windows program needs fold-equal resolution regardless of host FS."
```

---

### Task 7: Close §6b and correct the contract

**Files:**
- Modify: `rust/crates/vfs-node/test/primitives.test.mts`
- Modify: `rust/crates/vfs-provider/src/path.rs:14`

**Interfaces:**
- Consumes: Tasks 3–6.
- Produces: nothing downstream.

- [ ] **Step 1: Convert the §6b test**

`primitives.test.mts` contains case 6b, "a capitalised path in `memory()`", written as a **`test.fails`** — it currently asserts the hole is still open. With Task 3 landed it must pass normally.

Convert it to an ordinary `test`, and keep its explanatory comment, updating it from "known-failing — §6 casefold does not exist" to a note that the hole closed and how. **Do not delete the test.** It is the only record that the hole existed, and the plan's Global Constraints forbid removing it.

- [ ] **Step 2: Run the JS suite**

Run from `rust/crates/vfs-node`: `pnpm build && pnpm exec vitest run`
Expected: PASS with no `test.fails` reporting an unexpected pass.

- [ ] **Step 3: Correct `VPath`'s doc**

`path.rs:14` claims "original case preserved", which described only host-side callers. Replace with the truth and the contract:

```rust
/// A path as a provider sees it: normalized, forward-slash separated, no
/// leading slash, provider root is `""`.
///
/// **Case is the caller's, and a provider must not depend on it.** The shim
/// folds a vpath before sending it (`vfs-redirect`'s `match_canonical`), while
/// host-side callers — `vfs-embed`, `vfs-node`, this crate's conformance suite —
/// send the original spelling. A provider therefore resolves fold-equal names
/// identically unless it declares [`crate::CaseMatch::Sensitive`]. An earlier
/// version of this comment said "original case preserved", which was true of
/// only one of the two paths and is what spec §6b was.
```

- [ ] **Step 4: Full verification**

```bash
cargo test --no-fail-fast
cargo clippy --all-targets -- -D warnings
cargo check --target x86_64-unknown-linux-gnu -p vfs-director --all-targets
cargo tree -p vfs-director --target x86_64-unknown-linux-gnu | grep -E "vfs-win|windows-sys|vfs-shim|vfs-inject"
```
Expected: suite green; clippy clean; Linux check clean; the last command silent (increment 1's property still holds).

From the repository root:

```bash
bin/regen-protocol
git diff --exit-code resources/
```
Expected: no diff — this increment does not touch the protocol.

**Known hazard:** `vfs-directord`'s e2e tests fail when `TMP` contains an 8.3 short component, and hang intermittently. Both predate this plan and are unrelated to it. If either occurs, do not fix it here and do not report the suite as passing — record what happened and additionally run `cargo test -p vfs-provider -p vfs-compose -p vfs-cache -p vfs-director -p vfs-zip --no-fail-fast` as the meaningful signal.

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-node/test/primitives.test.mts rust/crates/vfs-provider/src/path.rs
git commit -m "fix: close spec 6b, and state VPath's real case contract

The 6b test.fails becomes an ordinary passing test — converted, not deleted, so
the record of the hole survives. VPath's doc claimed 'original case preserved',
which described host-side callers only while the shim sent folded paths; that
half-truth is what 6b was."
```

---

## Definition of Done

Mirrors the spec's section 9:

- [ ] `Capabilities::case` exists, defaults to `Insensitive`, is documented, and `weakest()` yields `Insensitive` only when every child is
- [ ] `assert_conformance` holds a provider to its declaration in both directions, with a non-ASCII case and an open-and-read case
- [ ] `memory` and `inline` resolve fold-equally and declare `Insensitive`; `subdir` honours the claim it inherits and gains the conformance test it never had
- [ ] `router.rs` has **zero diff** — it already folds via `glob::matches` and already derives `case` through `Capabilities::weakest`
- [ ] `DiskProvider` resolves fold-equally on non-Windows; confirmed by the Linux CI job
- [ ] `primitives.test.mts`'s §6b test passes as an ordinary test and still exists
- [ ] Windows suite green; `cargo clippy --all-targets -- -D warnings` clean; `bin/regen-protocol` no diff
- [ ] `vfs-embed` and `vfs-node` public surfaces unchanged apart from the new capability field
