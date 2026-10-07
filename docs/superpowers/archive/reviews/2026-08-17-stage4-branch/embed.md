# Review: `vfs-embed` seam + composition crates (`feat/stage4-embed`)

**Verdict:** The seam is real and most of the prose checks out, but §6's "hard error" is
bypassable through the second public mount entry point (the one the daemon actually uses),
and `MemoryProvider` reports success for directory `remove`/`rename` that do nothing —
both verified. Not mergeable as-is without at least the `set_root_mounts` gap addressed.

Environment: `cargo test -p vfs-embed -p vfs-compose` → all green (36 tests + 1 doctest).
`cargo clippy -p vfs-embed -p vfs-compose --all-targets -- -D warnings` → clean.
The Skyrim corpus at `C:\tmp\skyrimse.zip` **is** present on this machine, so the
corpus-gated tests did run here (249 MB byte-exact).

---

## Critical

### C1 — `set_root_mounts` does not enforce §6's `SeqRead` hard error; `mount_at` does
`rust/crates/vfs-embed/src/session.rs:519`

**VERIFIED by execution.** Probe results:

```
mount_at(SeqRead)        -> Err(-3)          // ST_BAD_REQUEST, as documented
set_root_mounts(SeqRead) -> Ok(())           // accepted
root 4 getattr(a.txt)    -> Ok(Some(Stat{kind:1,size:5}))
root 4 read_file(a.txt)  -> Err(-8)          // ST_NOT_SUPPORTED
```

That is precisely the outcome `mount_at`'s own doc comment (session.rs:479-489) says the
check exists to prevent, word for word: *"a session that composes cleanly, serves
`getattr` and `readdir` correctly, and fails every actual read."*

The doc also claims the placement rationale: *"Checked here rather than in each host: the
binding that has a friendly message for it is not the only surface that can reach
`mount_at`."* But `set_root_mounts` is a **second public surface that reaches the same
composition** and has no check — and it is the one the daemon takes on every source:
`rust/crates/vfs-directord/src/registry.rs:314` calls `set_root_mounts` from
`add_source`, once per `[[source]]`. `RootSources`/`stack_layers` do not filter it either
(`stack_layers` uses `Capabilities::weakest`, so a single `SeqRead` child drags the whole
layer stack to `SeqRead`; `MountGraph::capabilities` then takes the *strongest*, so one
`SeqRead` mount alone re-exports `SeqRead` to the root).

Consequence: the sentence in
`docs/superpowers/specs/2026-08-13-pluggable-providers-design.md:586` — *"The `SeqRead`
hard error now lives in `vfs_embed::Session::mount_at`"* — is true but incomplete in a way
that reads as a guarantee. `set_root_mounts` (and `stage_launch`'s staging slot, though
that is always a `DiskProvider`) is the hole.

### C2 — `MemoryProvider::remove` and `::rename` report `Ok(())` for directory operations they do not perform
`rust/crates/vfs-compose/src/memory.rs:258` and `:269`

**VERIFIED by execution.** With `sub/b.txt` present and `mkdir("sub")` recorded:

```
remove(sub)               -> Ok(())
getattr(sub) after remove -> Ok(Some(Stat{kind:2}))   // still a directory
getattr(sub/b.txt)        -> Ok(Some(...))            // child untouched

rename(sub -> sub2)       -> Ok(())
getattr(sub2/b.txt)       -> Ok(None)                 // nothing moved
getattr(sub/b.txt)        -> Ok(Some(...))            // still at the old name
```

`remove` only drops the `dirs` set entry; `stat_of`'s implicit-parent rule then re-derives
the directory from the surviving files, so the caller is told the removal succeeded and the
path still resolves. `rename` only moves the `dirs` entry and never rewalks the `files`
map, so a directory rename is a silent no-op that answers `Ok`. Both are wrong answers with
no diagnostic, in a provider now reachable two ways (`vfs_embed::MemoryProvider` and
`SourceSpec::Memory` via `vfs-source/src/lib.rs:58`).

The conformance suite cannot catch either: `assert_writable`
(`rust/crates/vfs-provider/src/conformance.rs:751-755`) only removes an **empty**
directory, and only renames a file. `memory.rs:299-306`'s own test claim — *"a writable
provider that has not passed the writable arm of the shared suite has not been shown to
work"* — is therefore load-bearing on a suite that does not cover these two cases.

Related, same file, lower severity: `mkdir("f.txt")` over an existing file returns `Ok(())`
while the path still stats as `KIND_FILE` (verified) — another silent no-op.

---

## Important

### I1 — `Session` has no write path, so every write in the crate's own tests goes through `kernel()`
`rust/crates/vfs-embed/src/session.rs:787` (read side exists; no write counterpart)

`Session` gained `read_file_at`, `readdir` and `getattr` on this branch specifically
because *"every host was reaching past the seam for it"* (session.rs:817-820). There is no
equivalent for writing: no `write_file_at`, no `open`/`write`/`close`. The consequence is
visible inside the crate — `tests/memory_provider_round_trip.rs:44,49,50` reaches
`session.kernel().open/write/close` to perform half of the very round trip the file exists
to demonstrate, and `tests/copy_on_write_composition.rs` and `tests/embed_api.rs` do the
same 14 more times. By the crate's own stated rule (*"if a host has to reach past this
crate, the fix belongs here"*), this is an unfilled gap; the seam guard (I5) cannot see it
because it only scans `vfs-node` and `vfs-launch`.

### I2 — `recompose` releases the roots lock before installing, so concurrent mount calls can silently install a stale graph
`rust/crates/vfs-embed/src/session.rs:589`

Reasoned from code, **not reproduced**. `mount_at`/`set_write_layer_at`/`set_root_mounts`/
`stage_launch` each mutate `self.roots` under the mutex, drop it, then call `recompose`,
which re-takes the lock, clones, drops it again, and only then calls `kernel.mount`. Two
threads on the same root can interleave as: A pushes → A reads `{A}` → B pushes → B reads
`{A,B}` → B installs `{A,B}` → A installs `{A}`. B's mount is lost, and worse,
`self.roots` still records it — so `composed_roots()`/`has_write_layer()` disagree with
what `Director` actually serves, silently. All four mutators take `&self` and use interior
mutability, which is an invitation to call them concurrently.

### I3 — `launch(wait: true)` holds `LAUNCH_ENV_LOCK` for the child's entire lifetime, so another session's `serve()` blocks on the game
`rust/crates/vfs-embed/src/session.rs:1028`

The guard is acquired before `vfs_inject::run_target_with_shim` and lives to the end of
`launch`, and `run_target_with_shim` blocks until exit when `detach: false`. `serve()`
takes the same lock (session.rs:874). So in exactly the multi-session host the lock's own
doc is written for, `Session::serve()` on session B blocks for the whole runtime of session
A's game. The doc block (session.rs:1-25 of the `LAUNCH_ENV_LOCK` comment, and
`launch`'s "Process-global environment" section) describes the lock as serialising *env
writes*; it does not say it serialises entire launches, and a host would not expect
`serve()` to be a multi-hour call.

### I4 — "Never wrap the upper in a `CachingProvider`" is documented as mandatory and enforced nowhere
`rust/crates/vfs-embed/src/session.rs:574`

`set_write_layer_at`'s doc calls the exemption not optional and names the symptom (*"a game
reading back its own edit as the original"*), and the daemon remembers it by hand
(`vfs-directord/src/registry.rs:343-356` deliberately does not cache). But
`Capabilities::cached()` passes `access` through unchanged, so a cached `ReadWrite` upper
satisfies the only check `set_write_layer_at` performs, and there is no capability bit or
marker that would let it be detected. No test covers it. This is the same class of gap
C1 describes — a rule stated in prose, enforced for one caller.

### I5 — the seam guard is narrower than its own headline, and trivially evadable
`rust/crates/vfs-embed/tests/embed_api.rs:389`

The test is titled `no_host_in_this_workspace_reaches_past_the_seam` and its doc opens
*"**no host in this workspace may reach past the seam.**"* It scans two crates
(`hosts = ["vfs-node", "vfs-launch"]`, line 399) for one literal string,
`concat!(".kernel", "()")` (line 392). Three problems:

* `vfs-directord/src/bin/skyrim-live.rs` is a host — it builds a `Session`, serves, and
  launches Skyrim — and it calls `.kernel()` **13 times** and names `vfs_director::` 6
  times. It is excluded from this guard (not in `hosts`) *and* from the daemon's guard,
  which deliberately skips `src/bin/` (`registry.rs:509-513`). The two guards' exemptions
  are complementary, so skyrim-live falls in the gap. The test body does acknowledge
  skyrim-live as a legitimate direct kernel user, so this is an overclaim in the headline
  rather than a hidden fact.
* The claim that the daemon's guard *"forbids naming any engine crate at all"* is not
  accurate: `vfs_source::` and `vfs_control::` are deliberately excluded (documented at
  `registry.rs:470-473`), and `src/bin/` is not scanned.
* A substring match on `.kernel()` is defeated by `Session::kernel(&s)` or
  `s . kernel ()`. For a guard whose entire value is being unfoolable, worth a stronger
  form.

I did verify the substantive claim: `vfs-launch/src/**` and `vfs-node/src/**` contain zero
`kernel()` calls and zero `vfs_director`/`vfs_directord` mentions, so the known instance in
the brief is genuinely fixed.

### I6 — `implicit_zip_directories_resolve_like_a_real_install` gates on a directory it deliberately no longer reads
`rust/crates/vfs-embed/tests/zip_serve_integrity.rs:326`

The test skips unless `C:\tmp\skyrim-native\Skyrim Special Edition` is a directory, then
does `let _ = native;` (line 376) and derives every expectation from the archive instead —
which its own comment explains at length (*"Ground truth is the archive itself, not an
extract on disk"*). So on a machine with the archive but no native extract, this test
prints `skip` and reports `ok` while asserting nothing, gated on a precondition it
consciously abandoned. The gate is stale; it should be `zip.is_file()` alone.

Same file, related: `real_archive_matches_native_extract` (:138) and
`data_listing_includes_the_master_plugins` (:405) also `return` on a missing corpus rather
than `#[ignore]`, so all three report green with zero coverage anywhere the corpus is
absent — including CI. `data_listing_includes_the_master_plugins` guards a bug actually
observed in production (empty load order, 2026-08-12), which is the worst one to have
silently vacuous.

### I7 — `read_file_at` allocates the whole file up front with no bound
`rust/crates/vfs-embed/src/session.rs:787` (and `KernelSource::read`, session.rs:262)

`let mut buf = vec![0u8; size as usize];` from a provider-reported size. A host calling
`read_file_at` on a 16 GB zip member (`vfs-zip` has a ZIP64 test archive that size) aborts
the process rather than erroring. Documented as an "occasional host-side full-file read",
but it is the only read the seam offers and it is what the Node binding's `readFile`
exposes to JavaScript.

---

## Minor

### M1 — `SKIP_CHUNK`'s justification misattributes §8c
`rust/crates/vfs-compose/src/seekable.rs:59`

*"64 KiB is the block size §8c measured as the best round-trip unit across the Node
bridge."* §8c measured 64 KiB as the best **cache block size** for `vfs-cache`
(spec line ~723, in a section about `store.rs` cloning whole blocks); it measured nothing
about round-trip units, and this constant is a discard buffer inside a pure-Rust combinator
that never touches the Node bridge. The number may well be fine; the cited evidence is not
evidence for it.

### M2 — `memory.rs`'s module doc presents §8's round trip as working, with no mention of the case hazard
`rust/crates/vfs-compose/src/memory.rs:6-7`

The header names `inis = vfs.memory({"Skyrim.ini": ...}); inis.read("Skyrim.ini")` as *"the
provider's whole reason to exist"* — the exact call the spec's own §6b (line 548) says
*"silently corrupts §8's own example"* and calls *"the highest-value gap left in the
catalog."* The file never mentions that it is case-sensitive or that a folded write from the
ring lands beside the seed. I reproduced the corruption end to end through the pure-Rust
seam:

```
seed "Skyrim.ini" = SEED; folded write to "skyrim.ini" (what the ring delivers)
read_file("Skyrim.ini")  -> "SEED"              // the host's own stale bytes
read_file("skyrim.ini")  -> "GAME-WROTE-THIS"
rejected_writes()        -> []
readdir("")              -> ["skyrim.ini"]      // the seed is not even listed
```

So nothing on this branch made the gap worse and nothing claims to have fixed it — the
handling elsewhere is honest (spec §6b, `vfs-node/src/primitives.rs:182`, and a proper
known-failing `test.fails` at `vfs-node/test/primitives.test.cts:604`). The two things
missing are this file's own doc, and any Rust-side record: `tests/memory_provider_round_trip.rs`
quotes §8's example verbatim but seeds and reads with **matching** case, so it is green
while §8 is broken, and there is no Rust equivalent of the Node `test.fails`.

### M3 — `Session::mount_at` never calls `Capabilities::validate()`
`rust/crates/vfs-embed/src/session.rs:466`

`vfs-provider/src/caps.rs:40` says validate is *"Called at construction."* In practice it
is called by the conformance suite, the JS-provider bridge
(`vfs-node/src/jsprovider.rs:953`), and two concrete leaves — never by the one central
place every provider in every host passes through. A Rust provider declaring
`{ReadWrite, immutable: true}` mounts silently, and `immutable` is the flag that authorises
persisting cache blocks across sessions.

### M4 — `RootSources::add` trims for the decision but stores the untrimmed prefix
`rust/crates/vfs-embed/src/sources.rs:62`

`mount_norm = mount.trim()` decides root-vs-prefix; the prefix branch then pushes
`mount.to_string()`. `normalize("  /Data  ")` yields the two-component prefix
`"  /Data  "`, which `strip_prefix` can never match — so `add("  /Data  ", …)` produces a
mount that silently serves nothing. The doc's "in any amount of surrounding whitespace"
clause is scoped to the root case, but the asymmetry is a trap.

### M5 — `SeekableProvider::reopen` leaves a dangling inner handle if the reopen fails
`rust/crates/vfs-compose/src/seekable.rs:120`

It closes `rec.inner` first (deliberately, per the comment) and only then reopens. If
`open` errors, `rec.inner` still names the closed handle, so every later `read_at` and the
eventual `close` operate on it. The close-first choice is documented; the failure path is
not. Also untested: nothing asserts `REOPEN_MASK` actually strips `OPEN_TRUNC`/`OPEN_EXCL`,
which the module doc calls *"catastrophic rather than merely wrong"* to get wrong.

### M6 — `MemoryProvider::open` answers `ST_BAD_REQUEST` for `OPEN_EXCL` on an existing path
`rust/crates/vfs-compose/src/memory.rs:185`

`ST_EXISTS` is the semantically right status and `exists()` is in the re-export list. It
matches `RwMemFixture` so it is consistent, and the suite only checks `is_err()`, which is
why neither is caught.

### M7 — `no_engine_crate_is_named_here`'s second needle is redundant
`rust/crates/vfs-embed/tests/embed_api.rs:351`

`concat!("vfs_", "directord")` can never match without `concat!("vfs_", "director")`
matching first. Harmless, but it reads as two independent checks.

### M8 — `copy_on_write_composition.rs` reaches around the re-export it is testing
`rust/crates/vfs-embed/tests/copy_on_write_composition.rs`

Uses `vfs_zip::ZipProvider`, `vfs_protocol::OPEN_CREATE` and `vfs_provider::ST_READ_ONLY`
directly, all of which `vfs_embed` re-exports (`vfs_embed::ZipProvider`,
`vfs_embed::OPEN_CREATE`, `vfs_embed::ST_READ_ONLY`). The file makes no seam claim so this
is not a false claim, but it does mean the crate's most production-shaped composition test
would not notice if those re-exports disappeared.

---

## Claims I checked and found accurate

Recording these so the negatives are not read as unexamined:

* **Staging precedence.** `recompose` puts `staging` first (session.rs:600-608) and
  `MountGraph::getattr`/`open`/`mkdir`/`remove`/`rename`/`set_attr` all walk
  `.iter().rev()` (`mount_graph.rs:131,222,307,318,333,349`), so first really is lowest
  precedence. `readdir` walks forward with `map.insert`, so last-registered wins there too —
  consistent. The claim, the inverted-daemon-ordering warning, and
  `a_staged_copy_must_not_shadow_curated_content_at_the_same_path`'s non-vacuity controls
  (`helper.exe` + the `readdir` check + the `set_root_mounts` rebuild) all hold.
* **`compose_root`'s `ST_BAD_REQUEST` conditions** — both are real, via
  `OverlayProvider::from_arcs` (overlay.rs:118) and `MountGraph::new`'s `normalize`.
* **`read_only_clamp` / `seekable` promotion**, and `readonly.rs`'s claim that the clamp is
  what makes the suite run the read cases: verified against
  `assert_conformance`'s `match caps.access` (conformance.rs:459-465).
* **`seekable(SeqFixture)` genuinely serves the positional suite.** I traced
  `assert_positional` through the cursor: the EOF, past-EOF, empty-buffer and unaligned
  cases each exercise a different branch of `seek_to`, including one backward reopen.
  `the_inner_provider_really_cannot_do_positional_reads` is a real non-vacuity control.
* **`declare_root(0)` repoints `virtual_root`** and stays out of `extra_roots` — asserted
  both directions, with a `debug_assert` in `extra_roots_env`.
* **`rejected_writes()` on a `ReadOnlyProvider` layer** — verified against
  `Director::open`'s `access < ReadWrite` gate (director.rs:135-141), and the test takes
  a lock for the process-global table rather than assuming test order.
* **`vfs-embed`'s `zip` feature is not cosmetic** — `vfs-director` carries `vfs-zip` as a
  *dev*-dependency only, so `--no-default-features` really does drop it.
* **`RootSources` rules 1 and 2**, `stack_layers`' stable sort, and the
  `prefixed_sources_alone_produce_no_root_mount` edge case: all correct as documented.
* **`empty_tree_snapshot`** decodes to 128 bytes with the `SSFV` magic and version 1.
