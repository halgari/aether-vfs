# aether-vfs: extraction design

**Date:** 2026-07-25
**Status:** Approved

## What

A standalone JVM Clojure library extracted from mauvi-clj's `pulsar.vfs` layer:
a provider-based virtual filesystem (the software definition of files) with a
Linux FUSE mount adapter (jnr-fuse, with an FFM zero-copy read path) and a
Proton launch runtime (run a Windows exe under Proton from a directory, grep
its logs, tear it down).

A future Windows counterpart will be completely new code (Rust) with a similar
runtime; it is explicitly out of scope here and imposes no design constraints
on this extraction.

## Scope decisions (as approved)

- **Faithful copy.** Rename-only port; no behavior changes, no restructuring,
  no speculative multi-module layout, no interface spec doc.
- **mauvi-clj is untouched.** It keeps its own `pulsar.vfs`; no dependency
  wiring, no deletions. Reconciling mauvi onto this library is a separate
  future task.
- **`snapshot.clj` stays in mauvi.** It is coupled to mauvi's chunk store
  (`pulsar.source` ChunkReader protocols); aether-vfs ships no chunk seam.
- **The Proton half of `pulsar.steam.launch` moves** (`proton-command`,
  `launch-proton!`, `teardown!`, `wineserver-path`, log grepping, path
  defaults). The Steam half (`stream-mount!`, `verify-file`, `find-file`,
  `locator-path`) stays in mauvi.

## Layout

```
aether-vfs/
├── deps.edn          clojure 1.12, jnr-fuse 0.5.8; :test alias (cognitect
│                     test-runner, --enable-native-access=ALL-UNNAMED)
├── README.md         what it is, mount example, Proton example, test command
├── src/aether/vfs/
│   ├── error.clj     errno mapping + raise/category (absorbed from
│   │                 pulsar.store.error); error key :aether.vfs/error
│   ├── types.clj     paths, Meta/DirEntry/Opened, open-flag helpers
│   ├── provider.clj  Provider / ReadInto / Writable protocols + read-only wrappers
│   ├── router.clj, compose.clj, read_pool.clj, inode.clj
│   ├── fuse.clj      jnr-fuse adapter + FFM zero-copy path
│   ├── proton.clj    proton-command, launch-proton!, teardown!, wineserver-path,
│   │                 drm-failures/reached-markers, default-steam-root/proton-path
│   └── providers/    inline, layered, overlay, passthrough, fsutil
└── test/aether/vfs/  ported tests + test_util.clj (tmp-dir, error-category)
```

## Renames

- Namespaces: `pulsar.vfs.*` → `aether.vfs.*`; `pulsar.store.error`'s
  `raise`/`category` fold into `aether.vfs.error`.
- Error key: `:pulsar/error` → `:aether.vfs/error`.
- Env vars: `MAUVI_NO_READ_INTO` → `AETHER_VFS_NO_READ_INTO`,
  `MAUVI_MAX_FUSE_READS` → `AETHER_VFS_MAX_FUSE_READS`,
  `MAUVI_PROTON_PATH` → `AETHER_VFS_PROTON_PATH`.
- Docstrings referencing mauvi-internal design docs (D-numbers, "Plan-1",
  the Rust vfsd) may be lightly trimmed to stand alone; semantics unchanged.

## Tests & verification

Ported: `types`, `router`, `error`, `inode`, `read-pool`, `providers`,
`overlay`, `mount` (real `/dev/fuse` mount; self-skips without it), plus
`compose` with its snapshot-backed cases rewritten against `inline-provider`
(same layering assertions, no chunk store), plus a `proton` test carrying the
pure `proton-command` invocation test from mauvi's `launch_test.clj`.

Staying in mauvi: `read_into_test`, `snapshot_test`, `launch_live_test`
(chunk-store/Steam-coupled).

**Done means:** `clojure -M:test` passes in aether-vfs on this machine,
including the real-mount test.
