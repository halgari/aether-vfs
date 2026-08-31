# aether-vfs — the case-fold contract

**Goal:** make case-insensitive resolution a **declared property of a provider**
rather than an undocumented convention applied unevenly at two different
boundaries. Closes spec §6b, and is the prerequisite that lets a FUSE adapter
pass paths through unfolded instead of reimplementing the shim's folding a
second time.

**Status:** proposed, 2026-08-31. Increment 2 of the Linux portability arc;
increment 1 (`2026-08-31-linux-fuse-proton-portability-design.md`) is merged.

---

## 1. Why

Two entry points disagree today about what a vpath *is*.

| entry point | spelling that reaches the provider |
|---|---|
| shim / ring | **folded** — `vfs-redirect`'s `match_canonical` returns "the folded remainder components" |
| host-side (`vfs-embed`, `vfs-node`, conformance) | **original case** — zero `fold` calls in `session.rs` or `vfs-node/src/lib.rs` |

`VPath`'s own doc (`vfs-provider/src/path.rs:14`) says *"original case
preserved"*, which describes only the second. The consequence reaches disk:
`vfs-shim/src/engine.rs:1046` — *"Overlay copy of data/foo.esp (folded
components on disk)"*.

**This is what §6b actually is.** The `test.fails` in `primitives.test.mts`
("a capitalised path in `memory()`") is usually described as "`memory()` is
case-sensitive". The real defect is that the same provider can be asked for
`Data/A.esp` by a host and `data/a.esp` by the shim, and nothing in the type
system, the trait, or the capability model says which to expect. `memory()` is
merely the first provider where the disagreement became visible — its only
occurrence of the word "fold" is a doc comment about `RtlNtStatusToDosError`,
not case folding at all.

Measured inventory of who folds with `vfs_core::fold` today:

| folds | does not fold |
|---|---|
| `vfs-director/src/disk.rs`, `mount_graph.rs` | `vfs-compose/src/memory.rs` |
| `vfs-compose/src/{layered,overlay,seekable,glob}.rs` | `vfs-compose/src/inline.rs` |
| `vfs-zip/src/backend.rs` (`by_fold` index) | `vfs-compose/src/router.rs` |
| | `vfs-compose/src/subdir.rs` |

`readonly.rs` is a pass-through wrapper and correctly has no opinion.

**Why this blocks Linux.** Wine resolves case-insensitively by scanning
directories, using ext4 case-folding as a fast path; `ciopfs` exists precisely
to give Wine case-insensitive lookup over FUSE. A FUSE adapter that passes the
kernel's spelling through — which is what we want, so Linux does not
reimplement the shim's folding — is correct **only if the provider graph beneath
it resolves case-insensitively**. Over `memory()` or `inline()` as they stand
today, any capitalised path fails.

## 2. What changes, and what deliberately does not

**The contract:** a vpath carries the caller's spelling. Providers resolve names
case-insensitively. Both existing entry points then work unchanged — the shim's
folded spelling and a host's original spelling both resolve to the same file.

**What does not change: the wire.** An earlier framing of this work had the shim
stop folding before send, making the vpath uniformly original-case. That is a
**wire-visible** change (`vfs-core/src/casefold.rs` says so explicitly: "a change
here is a wire-visible change"), and it would also move the overlay's on-disk
layout, which existing users' overlay directories depend on. Rejected: the
provider-level guarantee is strictly more general, costs no protocol change, and
carries no Windows risk. The shim keeps folding; that becomes an
implementation detail of one delivery adapter rather than a contract everything
below must infer.

**Four moves:**

1. `Capabilities` gains an explicit declaration of case behaviour.
2. `assert_conformance` gains cases that hold a provider to its declaration.
3. The four in-tree providers that should be case-insensitive and are not —
   `memory`, `inline`, `router`, `subdir` — become so, keyed the way
   `vfs-zip`'s `by_fold` already does it.
4. `VPath`'s doc comment is corrected to state the contract rather than a
   half-truth.

## 3. The capability

```rust
/// How this provider matches a name it is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseMatch {
    /// Fold-equal names resolve to the same entry. `vfs_core::fold` is the
    /// definition of fold-equal — not `to_ascii_lowercase`, and not "the OS
    /// will sort it out".
    Insensitive,
    /// Byte-exact names only. Correct for a provider over a case-sensitive
    /// store that has not indexed for folding; **not** safe under a FUSE mount
    /// serving a Windows program.
    Sensitive,
}
```

added to `Capabilities` as `pub case: CaseMatch`, defaulting to `Insensitive`
in `Capabilities::read_only()` — the common case, and the one a Windows-facing
VFS must provide.

**The declaration is a statement of fact, not a request.** In this increment it
does exactly one thing: select conformance cases. It does not make the Director
wrap, reject, or reconfigure anything.

Two things it deliberately does **not** do yet, both YAGNI until a caller needs
them:

- **No auto-wrapping.** A silent `casefold(provider)` wrapper would hide exactly
  the mismatch this declaration exists to surface.
- **No mount-time refusal.** It would be reasonable for a future FUSE mount to
  refuse a `Sensitive` provider outright, since a Windows program over one is
  broken by construction. That check belongs to increment 3, where there is a
  mount to attach it to; adding it here would mean writing a policy with no
  consumer.

## 4. `DiskProvider` is the interesting one

`disk.rs` folds in two places but leans on the OS for the rest: NTFS matches
case-insensitively on its own, so on Windows the provider is `Insensitive`
without trying. On Linux over ext4 it is `Sensitive`, and nothing in the code
says so.

Declaring it honestly per-platform would be truthful and useless — it would
make `DiskProvider` unusable under a Linux FUSE mount, which is the whole point
of the arc. So `DiskProvider` must resolve case-insensitively **itself** on
non-Windows targets: a fold-scan of the containing directory on a miss, which is
what Wine does and what `ciopfs` was built to avoid doing repeatedly.

Two consequences to be honest about:

- **A miss now costs a `readdir`.** Only on the miss path, and only where the OS
  does not fold, but it is a real cost on a directory with many entries. Cache
  nothing in this increment; measure before optimising.
- **`vfs_core::fold` is not NTFS-case-equivalence.** `casefold.rs` warns that
  `İ` (U+0130) folds to a genuinely different name, so "NTFS is case-insensitive,
  therefore the folded spelling names the same file" is not sound. A fold-scan
  must compare with `fold`, and must not assume the folded spelling can be
  handed to the filesystem.

This is verifiable in CI rather than locally, because increment 1 put
`cargo test -p vfs-director` on a real Linux host.

## 5. Conformance

`assert_conformance` already selects cases by declared capability
(`conformance.rs:3`: "Cases are selected by the provider's *declared*
capabilities"). Case behaviour joins that mechanism:

- For `CaseMatch::Insensitive`: seed `Data/A.esp`, then require `getattr`,
  `open`, and `readdir`-membership to succeed for `data/a.esp`, `DATA/A.ESP`
  and `dAtA/a.EsP`. On a writable provider, require that writing through a
  differently-cased spelling hits the **same** entry rather than creating a
  sibling — which is precisely the §6b failure.
- For `CaseMatch::Sensitive`: require that a differently-cased spelling does
  **not** resolve. A provider declaring `Sensitive` while behaving
  insensitively is also a contract violation and is caught here.
- Include one non-ASCII case (`Über/A.esp` vs `über/a.esp`), because
  `to_ascii_lowercase` passes an ASCII-only suite and this project has already
  shipped that bug once — `casefold.rs` records it: `Data/ÜBER/a.esp` crossed
  the ring folded while every index below was keyed unfolded.

Because the JS binding runs `assertConformance` against JavaScript providers,
these cases bind third-party providers too, which is the point of putting the
guarantee in the capability model rather than in a Rust-side helper.

## 6. What must not regress

- **Windows behaviour, exactly.** No wire change, no change to the shim's
  folding, no change to the overlay's on-disk layout.
- **`bin/regen-protocol` produces no diff.** This increment does not touch the
  protocol.
- **The `test.fails` in `primitives.test.mts` must be converted, not deleted.**
  It pins §6b and is currently *expected* to fail; when the hole closes it must
  become an ordinary passing test in the same commit. Deleting it would remove
  the only evidence the hole ever existed.
- **`vfs-provider` keeps zero dependencies.** `CaseMatch` is a plain enum.

## 7. Scope

**In:** the capability, the conformance cases, the four in-tree providers,
`DiskProvider`'s non-Windows fold-scan, and the `VPath` doc correction.

**Out:** `vfs-fuse` (increment 3). Linux `launch()`, the GE-Proton provisioner
and prefix path rerouting (increment 4). Auto-wrapping a `Sensitive` provider.
Any change to the ring protocol or the shim's folding. Caching the fold-scan.

## 8. Risks

- **A provider declaring `Insensitive` that is not.** The conformance cases are
  the mitigation, but only for providers actually run through them. A composed
  graph is only as case-insensitive as its least-insensitive leaf, and nothing
  computes that. Worth a follow-up; not solved here.
- **The fold-scan changes `DiskProvider`'s failure mode on Linux** from
  "not found" to "found, later than expected". Any test asserting a miss on a
  differently-cased path on Linux would flip. There are none today.
- **`subdir` and `router` fold prefixes, not just leaves.** Getting a prefix
  comparison wrong is how `strip_prefix` and `mount_child_name` broke before
  (`casefold.rs`): the fold is **not length-preserving**, so never slice a
  folded string by an offset measured on the unfolded one. Walk components.

## 9. Definition of done

1. `Capabilities::case` exists, defaults to `Insensitive`, and is documented.
2. `assert_conformance` holds a provider to its declaration, including a
   non-ASCII case and a write-through-different-case case.
3. `memory`, `inline`, `router`, `subdir` resolve case-insensitively and pass
   the new cases.
4. `DiskProvider` resolves case-insensitively on non-Windows; verified by
   `cargo test -p vfs-director` on the Linux CI job.
5. `primitives.test.mts`'s §6b `test.fails` is now an ordinary passing test.
6. Windows: full suite green, `cargo clippy --all-targets -- -D warnings` clean,
   `bin/regen-protocol` no diff.
7. `vfs-embed` and `vfs-node` public surfaces unchanged apart from the new
   capability field.

---

## Forward context — decisions already taken for increment 4

Recorded here so they are not re-litigated or lost. Not in scope for this
increment.

**Proton is GE, never stock, and this is a hard requirement.** umu-launcher's
`PROTONPATH` defaults to **UMU-Proton**, which is *Valve's* stable Proton with
umu compatibility added — i.e. doing nothing silently selects the wrong Proton.
The Linux launch path must therefore assert that the Proton it resolved is the
intended GE build and refuse to launch otherwise, in the spirit of
`assertReleaseAddon()` refusing to benchmark a debug addon. A wrong-Proton run
would otherwise look like success while behaving subtly differently.

**aether-vfs provisions GE-Proton itself.** `PROTONPATH=GE-Proton` (the
codename) auto-downloads, but into `$HOME/.local/share/Steam/compatibilitytools.d`
— shared Steam data, with no documented way to redirect it. So aether-vfs
fetches the latest GE-Proton from `GloriousEggroll/proton-ge-custom` into its
own data directory and passes an **absolute** `PROTONPATH`, which is the one
form that performs no download. Requirements: verify the published `sha512sum`;
record the resolved version per session so a working configuration is
reproducible; cache so later launches are offline; allow a pin.

**Path rerouting comes from owning the prefix.** `WINEPREFIX` is ours, so
`dosdevices/c:` — an ordinary symlink to `../drive_c` — can point at a FUSE
mount. That reroutes *any* Windows path the game opens, not just paths under one
mountpoint, which is the Linux equivalent of the shim's in-place hooking. It
also offers containment Windows does not have: `dosdevices/z:` maps to `/` by
default and can be removed or repointed.

**Two umu prerequisites remain unverified and must be settled before increment 4
is planned.** First, umu is a repackaged `SteamLinuxRuntime_sniper` and an open
issue reports it downloading `steamrt3` even with `UMU_NO_RUNTIME=1`, so
fully-offline operation is not yet established. Second, because sniper runs the
game in a container namespace, a FUSE mount must propagate *into* that
namespace; that is unproven. Both are cheap to test in WSL and both can
invalidate parts of increment 4's design.
