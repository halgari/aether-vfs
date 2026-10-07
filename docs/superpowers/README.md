# Design docs index

Specs, plans and reviews for aether-vfs, oldest first within each group. Every
doc has one row. Written for the 2026-10-07 cleanup pass (finding E12).

## Archive convention

- **Current** docs sit in `specs/` and `plans/`. A doc is current when it describes something
  still true of the code or still in progress, or when it is the reference for a live crate.
  When unsure, a doc stays current and its summary says why.
- **Historical** docs describe completed or superseded work. They were moved with `git mv`
  into `archive/specs/`, `archive/plans/` and `archive/reviews/`, keeping their file names. Their
  content was not edited, so their `Status:` lines are as stale as when they were written
  (many say "proposed" or "plan pending" for work that shipped). Trust this index, and the code,
  over a status line.
- The old `rust/docs/superpowers/` tree (pre-extraction, July 2026) is folded in: five designs
  that are still live moved to `specs/`, the rest to the archive.
- Links inside archived docs, and from one doc to another, may be broken after the move. They are
  historical and were left alone. Use the file name to find the target here. Links from READMEs,
  `rust/docs/*.md`, code comments and `Cargo.toml` were fixed (they point at current docs).
- To archive a doc: `git mv` it into the matching `archive/` folder, and flip its row below.

## Current (19)

| Date | Doc | Kind | Status | Summary |
|---|---|---|---|---|
| 2026-07-13 | [vfs-ipc Control Ring — Design Spec](specs/2026-07-13-vfs-ipc-control-ring-design.md) | spec | current | Shared-memory control ring; vfs-ipc is still built on it. |
| 2026-07-14 | [Dual-Layer Inject Handoff — Design Spec](specs/2026-07-14-dual-layer-inject-handoff-design.md) | spec | current | Pre-init early payload plus full shim via a spin-gate handoff; implemented in vfs-inject. |
| 2026-07-14 | [Pre-init Injection — Reflective-map + RIP-redirect](specs/2026-07-14-preinit-injection-design.md) | spec | current | Reflective-map plus RIP-redirect pre-init injection; vfs-payload and vfs-inject use it. |
| 2026-07-14 | [Zip-Backed Layers — Serve Mod Files Directly From Stored ZIP Archives](specs/2026-07-14-zip-backed-layers-design.md) | spec | current | Serving mod files straight from stored ZIP archives; vfs-zip. |
| 2026-07-15 | [Director-Centric Userland FUSE + Thin Shim — Design Spec](specs/2026-07-15-director-fuse-thin-shim-design.md) | spec | current | Director-centric RPC with a thin shim. The FUSE half is superseded by the wine-hosted shim, but the director/thin-shim split is still the architecture. |
| 2026-08-11 | [Director Daemon Rework — Design Spec](specs/2026-08-11-director-daemon-rework-design.md) | spec | current | Director daemon as the one source of truth; shim becomes a thin client. Implemented through M4; the live daemon architecture. |
| 2026-08-13 | [No Bypass and Real Roots — Design Spec](specs/2026-08-13-no-bypass-and-real-roots-design.md) | spec | current | Invariant that no file access bypasses the director, plus real-root virtualisation. Gates 1-5 and 2b executed; still the contract the shim enforces (header says "not implemented" but is stale). |
| 2026-08-13 | [Pluggable Providers and the Embeddable Library — Design Spec](specs/2026-08-13-pluggable-providers-design.md) | spec | current | Provider contract, crate table and embeddable library; the reference for vfs-provider, vfs-compose and vfs-embed. Its Node and cache sections are historical. |
| 2026-08-31 | [aether-vfs — the case-fold contract](specs/2026-08-31-case-fold-contract-design.md) | spec | current | Single case-folding contract across layers and the wire. |
| 2026-08-31 | [aether-vfs — Linux portability: make the Director OS-agnostic](specs/2026-08-31-linux-fuse-proton-portability-design.md) | spec | current | Making the director OS-agnostic for Linux and Proton. Its FUSE half is superseded by the wine-hosted shim. |
| 2026-09-01 | [Linux delivery via the Wine-hosted shim — design](specs/2026-09-01-wine-hosted-shim-design.md) | spec | current | Linux delivery by running the Windows shim inside Wine/Proton; the live Linux launch path. |
| 2026-09-23 | [Block Store Design](specs/2026-09-23-block-store-design.md) | spec | current | Content-addressed block store design; vfs-block-store. |
| 2026-09-29 | [The `vfs` CLI and daemon on Linux, and rooted launches — design](specs/2026-09-29-linux-cli-design.md) | spec | current | The vfs CLI and daemon on Linux, and rooted launches. |
| 2026-09-29 | [`vfs-storage`: the block store as cache and layer storage — design](specs/2026-09-29-vfs-storage-design.md) | spec | current | The block store as cache and layer storage; vfs-storage. |
| 2026-10-05 | [Registry overlay — implementation plan](plans/2026-10-05-registry-overlay.md) | plan | current | Implementation plan for the registry overlay. Kept current as the reference for how it was built and its follow-ups. |
| 2026-10-05 | [aether-vfs — Registry overlay: per-profile copy-on-write of registry keys](specs/2026-10-05-registry-overlay-design.md) | spec | current | Per-profile copy-on-write registry overlay (overlay.reg); implemented. |
| 2026-10-07 | [aether-vfs cleanup pass: implementation plan](plans/2026-10-07-cleanup.md) | plan | current | Task-by-task plan for the cleanup pass (in progress). |
| 2026-10-07 | [Cleanup audit reports (director, embed, shim, storage)](specs/2026-10-07-cleanup-audit/) | audit | current | Four audit reports (director, embed, shim, storage) holding the detail behind every cleanup finding ID. |
| 2026-10-07 | [aether-vfs: code cleanup pass](specs/2026-10-07-cleanup-design.md) | spec | current | The 2026-10 code cleanup pass: scope, streams, rules. |

## Historical (77 rows, in `archive/`)

| Date | Doc | Kind | Status | Summary |
|---|---|---|---|---|
| 2026-07-13 | [Spike B — Instrumentation-Callback Timing — Implementation Plan](archive/plans/2026-07-13-spike-b-instrumentation-callback.md) | plan | historical | Spike B plan. Shelved. |
| 2026-07-13 | [Spike B — Instrumentation-Callback Timing — Design Spec](archive/specs/2026-07-13-spike-b-instrumentation-callback-timing-design.md) | spec | historical | Instrumentation-callback timing spike. Shelved in favour of pre-init injection. |
| 2026-07-13 | [vfs-core Foundation Implementation Plan](archive/plans/2026-07-13-vfs-core-foundation.md) | plan | historical | vfs-core plan. Executed. |
| 2026-07-13 | [vfs-core Foundation — Design Spec](archive/specs/2026-07-13-vfs-core-foundation-design.md) | spec | historical | First vfs-core design. Executed; architecture.md and the code are the reference now. |
| 2026-07-13 | [VFS Directory Enumeration Hook (Slice F) Implementation Plan](archive/plans/2026-07-13-vfs-directory-enumeration-hook.md) | plan | historical | Directory enumeration hook plan. Executed. |
| 2026-07-13 | [VFS Directory Enumeration Hook (Slice F) — Design](archive/specs/2026-07-13-vfs-directory-enumeration-hook-design.md) | spec | historical | Directory enumeration hook (slice F). Executed. |
| 2026-07-13 | [Directory Merge Transform Implementation Plan](archive/plans/2026-07-13-vfs-directory-merge.md) | plan | historical | Directory merge plan. Executed. |
| 2026-07-13 | [Directory Merge Transform — Design Spec](archive/specs/2026-07-13-vfs-directory-merge-design.md) | spec | historical | Directory merge transform (slice C). Executed. |
| 2026-07-13 | [First-Class Tombstones Implementation Plan](archive/plans/2026-07-13-vfs-first-class-tombstones.md) | plan | historical | Tombstones plan. Executed. |
| 2026-07-13 | [First-Class Tombstones — Design Spec](archive/specs/2026-07-13-vfs-first-class-tombstones-design.md) | spec | historical | First-class tombstones. Executed. |
| 2026-07-13 | [VFS Handle Identity (Slice G) Implementation Plan](archive/plans/2026-07-13-vfs-handle-identity.md) | plan | historical | Handle identity plan. Executed. |
| 2026-07-13 | [VFS Handle Identity (Slice G) — Design](archive/specs/2026-07-13-vfs-handle-identity-design.md) | spec | historical | Handle identity (slice G). Executed. |
| 2026-07-13 | [Hooks: Path-Based Attribute Queries Implementation Plan](archive/plans/2026-07-13-vfs-hook-attributes.md) | plan | historical | Attribute hooks plan. Executed. |
| 2026-07-13 | [Hooks: Path-Based Attribute Queries — Design Spec](archive/specs/2026-07-13-vfs-hook-attributes-design.md) | spec | historical | Path-based attribute query hooks (slice E). Executed. |
| 2026-07-13 | [Hook: Honor Deny (Tombstone Hiding) Implementation Plan](archive/plans/2026-07-13-vfs-hook-deny.md) | plan | historical | Deny hook plan. Executed. |
| 2026-07-13 | [Hook: Honor Deny (Tombstone Hiding) — Design Spec](archive/specs/2026-07-13-vfs-hook-deny-design.md) | spec | historical | Honouring Deny by hiding tombstoned paths (slice D). Executed. |
| 2026-07-13 | [vfs-inject Cross-Process End-to-End Implementation Plan](archive/plans/2026-07-13-vfs-inject-end-to-end.md) | plan | historical | Injection end-to-end plan. Executed. |
| 2026-07-13 | [vfs-inject Cross-Process End-to-End — Design Spec](archive/specs/2026-07-13-vfs-inject-end-to-end-design.md) | spec | historical | Cross-process injection end-to-end. Executed. |
| 2026-07-13 | [vfs-ipc Control Ring Implementation Plan](archive/plans/2026-07-13-vfs-ipc.md) | plan | historical | vfs-ipc control ring plan. Executed. |
| 2026-07-13 | [Read-Path Decision Transforms Implementation Plan](archive/plans/2026-07-13-vfs-read-path-transforms.md) | plan | historical | Read-path transforms plan. Executed. |
| 2026-07-13 | [Read-Path Decision Transforms — Design Spec](archive/specs/2026-07-13-vfs-read-path-transforms-design.md) | spec | historical | Read-path decision transforms (slice B). Executed. |
| 2026-07-13 | [vfs-redirect Decision Core Implementation Plan](archive/plans/2026-07-13-vfs-redirect-decision-core.md) | plan | historical | vfs-redirect plan. Executed. |
| 2026-07-13 | [vfs-redirect Decision Core — Design Spec](archive/specs/2026-07-13-vfs-redirect-decision-core-design.md) | spec | historical | vfs-redirect decision core. Executed. |
| 2026-07-13 | [vfs-server Request Dispatch Implementation Plan](archive/plans/2026-07-13-vfs-server.md) | plan | historical | vfs-server plan. Crate deleted. |
| 2026-07-13 | [vfs-server Request Dispatch — Design Spec](archive/specs/2026-07-13-vfs-server-design.md) | spec | historical | vfs-server request dispatch. The crate was deleted. |
| 2026-07-13 | [vfs-shared Snapshot Layout Implementation Plan](archive/plans/2026-07-13-vfs-shared.md) | plan | historical | vfs-shared plan. Executed. |
| 2026-07-13 | [vfs-shared Snapshot Layout — Design Spec](archive/specs/2026-07-13-vfs-shared-design.md) | spec | historical | vfs-shared snapshot layout. Executed. |
| 2026-07-13 | [vfs-shim NtCreateFile Hook Implementation Plan](archive/plans/2026-07-13-vfs-shim-ntcreatefile-hook.md) | plan | historical | NtCreateFile hook plan. Executed. |
| 2026-07-13 | [vfs-shim NtCreateFile Hook — Design Spec](archive/specs/2026-07-13-vfs-shim-ntcreatefile-hook-design.md) | spec | historical | First NtCreateFile hook. Executed. |
| 2026-07-13 | [vfs-win Shared Memory Implementation Plan](archive/plans/2026-07-13-vfs-win-shared-memory.md) | plan | historical | vfs-win shared memory plan. Executed. |
| 2026-07-13 | [vfs-win Shared Memory — Design Spec](archive/specs/2026-07-13-vfs-win-shared-memory-design.md) | spec | historical | vfs-win shared memory. Executed. |
| 2026-07-13 | [VFS Write Path (Overlay + Whiteouts) — Design](archive/specs/2026-07-13-vfs-write-path-design.md) | spec | historical | Disk overlay with whiteouts. Superseded by the director write path. |
| 2026-07-14 | [Dual-Layer Inject Handoff — Implementation Plan](archive/plans/2026-07-14-dual-layer-inject-handoff.md) | plan | historical | Dual-layer handoff plan. Marked implemented 2026-07-14. |
| 2026-07-14 | [Pre-init Injection — Implementation Plan](archive/plans/2026-07-14-preinit-injection.md) | plan | historical | Pre-init injection plan stub; points at the design. |
| 2026-07-14 | [Zip-Backed Layers Implementation Plan](archive/plans/2026-07-14-zip-backed-layers.md) | plan | historical | Zip-backed layers plan. Executed. |
| 2026-07-15 | [Director-Centric FUSE Thin Shim Implementation Plan](archive/plans/2026-07-15-director-fuse-thin-shim.md) | plan | historical | Director/FUSE thin shim plan. Executed; FUSE half superseded. |
| 2026-07-15 | [Userspace FUSE Director + C ABI — Implementation Plan](archive/plans/2026-07-15-userspace-fuse-director-c-abi.md) | plan | historical | C ABI plan. Superseded by vfs-embed. |
| 2026-07-15 | [Userspace FUSE Director + C ABI — Design Spec](archive/specs/2026-07-15-userspace-fuse-director-c-abi-design.md) | spec | historical | Userspace FUSE director plus C ABI. Superseded by vfs-embed and the wine-hosted shim. |
| 2026-07-25 | [aether-vfs Extraction Implementation Plan](archive/plans/2026-07-25-aether-vfs-extraction.md) | plan | historical | Extraction plan. Executed. |
| 2026-07-25 | [aether-vfs: extraction design](archive/specs/2026-07-25-aether-vfs-extraction-design.md) | spec | historical | Extracting aether-vfs from the Haskill repo. Executed. |
| 2026-07-26 | [M1 — Merge, Restructure & Anti-Drift Scaffold Implementation Plan](archive/plans/2026-07-26-m1-merge-and-anti-drift-scaffold.md) | plan | historical | M1 merge and anti-drift plan. Executed. |
| 2026-07-26 | [Unified cross-platform VFS: aether-vfs + vfs merge](archive/specs/2026-07-26-unified-cross-platform-vfs-design.md) | spec | historical | Merging the Rust engine under rust/ with the Clojure layer and anti-drift scaffold. Executed; the Clojure layer was later removed. |
| 2026-07-27 | [M2 — JVM FFM Ring Server (Read Path) Implementation Plan](archive/plans/2026-07-27-m2-jvm-ffm-ring-server.md) | plan | historical | M2 plan. Executed, JVM layer removed. |
| 2026-07-27 | [M2 — JVM FFM ring server (read path)](archive/specs/2026-07-27-m2-jvm-ffm-ring-server-design.md) | spec | historical | JVM FFM ring server, read path. Clojure/JVM layer removed. |
| 2026-07-27 | [M3 — JVM-driven injection, real hooks, end-to-end read](archive/specs/2026-07-27-m3-injection-real-hooks-design.md) | spec | historical | JVM-driven injection and real hooks. Superseded by the Rust-only launch path. |
| 2026-07-27 | [M3 Part 1 — Foundations + De-risk Spike Implementation Plan](archive/plans/2026-07-27-m3-part1-foundations-and-spike.md) | plan | historical | M3 part 1 plan. Executed, JVM layer removed. |
| 2026-07-27 | [M3 Part 2 — Launch Productionization Implementation Plan](archive/plans/2026-07-27-m3-part2-launch-productionization.md) | plan | historical | M3 part 2 plan. Executed, JVM layer removed. |
| 2026-07-28 | [M4 Part 1 — Minimal Write Proof Implementation Plan](archive/plans/2026-07-28-m4-part1-minimal-write.md) | plan | historical | M4 part 1 plan. Executed, JVM layer removed. |
| 2026-07-28 | [M4 Part 2 — Full Write Set Implementation Plan](archive/plans/2026-07-28-m4-part2-write-set.md) | plan | historical | M4 part 2 plan. Executed, JVM layer removed. |
| 2026-07-28 | [M4 — Write path (pure-ring, JVM overlay authoritative)](archive/specs/2026-07-28-m4-write-path-design.md) | spec | historical | Pure-ring write path with a JVM-authoritative overlay. Superseded by the director overlay. |
| 2026-07-30 | [M5 — Unified Entry + Packaging + Docs Implementation Plan](archive/plans/2026-07-30-m5-unified-entry.md) | plan | historical | M5 plan. Executed. |
| 2026-07-30 | [M5 — Unified Entry + Packaging + Docs (design)](archive/specs/2026-07-30-m5-unified-entry-design.md) | spec | historical | Unified entry, packaging and docs for the JVM-era milestones. Superseded. |
| 2026-08-11 | [Code review: M0–M4 director daemon rework](archive/reviews/2026-08-11-m0-m4-once-over.md) | review | historical | Code review of the M0-M4 director rework. Findings fixed. |
| 2026-08-13 | [Stage 1: Provider Contract Foundations — Implementation Plan](archive/plans/2026-08-13-stage1-provider-contract.md) | plan | historical | Stage 1 plan: provider contract. Executed. |
| 2026-08-13 | [Stage 2a-i: The Write Path — Implementation Plan](archive/plans/2026-08-13-stage2a-i-write-path.md) | plan | historical | Stage 2a-i plan: write path. Executed. |
| 2026-08-13 | [Stage 2a-ii Gate 1: Measure the Bypass — Implementation Plan](archive/plans/2026-08-13-stage2a-ii-gate1-measure.md) | plan | historical | Gate 1 plan: measure the bypass. Executed. |
| 2026-08-13 | [Stage 2a-ii Gate 2: Canonicalise and Close the Escapes — Implementation Plan](archive/plans/2026-08-13-stage2a-ii-gate2-canonicalise.md) | plan | historical | Gate 2 plan: canonicalise and close escapes. Executed. |
| 2026-08-14 | [Stage 2a-ii Gate 3: Virtualise the Roots — Implementation Plan](archive/plans/2026-08-14-stage2a-ii-gate3-virtualise.md) | plan | historical | Gate 3 plan: virtualise the roots. Executed. |
| 2026-08-14 | [Gate 4: Close the Write Fall-Through — Implementation Plan](archive/plans/2026-08-14-stage2a-ii-gate4-writes.md) | plan | historical | Gate 4 plan: close the write fall-through. Executed. |
| 2026-08-14 | [Stage 2b: Real Roots — Implementation Plan](archive/plans/2026-08-14-stage2b-real-roots.md) | plan | historical | Stage 2b plan: real roots. Executed. |
| 2026-08-15 | [Gate 5: Close the DRM Exceptions — Implementation Plan](archive/plans/2026-08-15-stage2a-ii-gate5-drm.md) | plan | historical | Gate 5 plan: close the DRM exceptions. Executed. |
| 2026-08-16 | [Stage 4: `vfs-embed` and the Node Binding — Implementation Plan](archive/plans/2026-08-16-stage4-embed-and-node.md) | plan | historical | Stage 4 plan: vfs-embed and the Node binding. Executed, Node half removed. |
| 2026-08-17 | [The Node Binding in TypeScript — Implementation Plan](archive/plans/2026-08-17-node-typescript-migration.md) | plan | historical | Node binding in TypeScript. Node binding removed. |
| 2026-08-17 | [Post-Review Follow-Ups — feat/stage4-embed](archive/plans/2026-08-17-post-review-followups.md) | plan | historical | Follow-ups from the stage 4 branch review. Executed. |
| 2026-08-17 | [Stage 4 branch pre-merge reviews](archive/reviews/2026-08-17-stage4-branch/) | review | historical | Five pre-merge review reports for the stage 4 branch (cache, docs, embed, node, shim). Findings handled by the post-review follow-ups plan. |
| 2026-08-19 | [aethervfs ESM Migration Implementation Plan](archive/plans/2026-08-19-esm-migration.md) | plan | historical | ESM migration plan. Node binding removed. |
| 2026-08-19 | [aethervfs — ESM migration design](archive/specs/2026-08-19-esm-migration-design.md) | spec | historical | ESM migration of the Node binding. vfs-node was removed. |
| 2026-08-31 | [Case-Fold Contract Implementation Plan](archive/plans/2026-08-31-case-fold-contract.md) | plan | historical | Case-fold contract plan. Executed. |
| 2026-08-31 | [Linux Portability Increment 1 Implementation Plan](archive/plans/2026-08-31-linux-portability-increment-1.md) | plan | historical | Linux portability increment 1. Executed. |
| 2026-09-01 | [Closing the NtQueryObject identity leak — Implementation Plan](archive/plans/2026-09-01-ntqueryobject-identity.md) | plan | historical | Closing the NtQueryObject identity leak. Executed. |
| 2026-09-01 | [GE-Proton acquisition — Implementation Plan](archive/plans/2026-09-01-proton-acquisition.md) | plan | historical | GE-Proton acquisition (vfs-proton). Executed. |
| 2026-09-01 | [The Wine serve path — Implementation Plan](archive/plans/2026-09-01-wine-serve-path.md) | plan | historical | Wine serve path. Executed. |
| 2026-09-01 | [Wine-hosted shim, increment 1: the transport — Implementation Plan](archive/plans/2026-09-01-wine-transport.md) | plan | historical | Wine-hosted shim increment 1: the transport. Executed. |
| 2026-09-02 | [`Session::launch` on Linux — Implementation Plan](archive/plans/2026-09-02-session-launch.md) | plan | historical | Session::launch on Linux. Executed. |
| 2026-09-23 | [Block Store Implementation Plan](archive/plans/2026-09-23-block-store.md) | plan | historical | Block store plan. Executed. |
| 2026-09-29 | [The `vfs` CLI on Linux, and rooted launches — Implementation Plan](archive/plans/2026-09-29-linux-cli.md) | plan | historical | vfs CLI plan. Executed. |
| 2026-09-29 | [`vfs-storage` Implementation Plan](archive/plans/2026-09-29-vfs-storage.md) | plan | historical | vfs-storage plan. Executed. |
