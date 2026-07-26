# aether-vfs

A provider-based virtual filesystem for the JVM. A `Provider` answers
`lookup`/`readdir`/`open-file`/`read-at`/`write-at`/`release-handle` for a
tree of virtual paths; a `router` maps glob patterns to providers; and
`aether.vfs.fuse` mounts the whole thing as a real Linux FUSE filesystem
in-process via [jnr-fuse](https://github.com/SerCeMan/jnr-fuse), with an FFM
zero-copy read path (`java.lang.foreign`) that lets a provider write straight
into the kernel's own read buffer instead of bouncing through the Java heap.
A `proton` namespace launches Windows executables against a mount under
Proton. It was extracted from the mauvi mod manager's storage layer. Linux
only today — a Rust-based Windows counterpart exposing the same interfaces is
planned as a separate project.

## Mount example

```clojure
(require '[aether.vfs.fuse :as fuse]
         '[aether.vfs.providers.inline :as inline])

(let [root  (inline/inline-provider [["/hello.txt" (.getBytes "hi") 0644]])
      guard (fuse/mount root "/tmp/my-mount")]
  (try
    (slurp "/tmp/my-mount/hello.txt") ;=> "hi"
    (finally
      (.close guard))))
```

`fuse/mount` proxies FUSE callbacks straight onto the `Provider` protocol and
returns a `java.io.Closeable` guard; closing it unmounts. `inline/inline-provider`
takes `[[virtual-path bytes perm] …]` and serves it read-only from RAM — no
store, no cache, no disk — which makes it the simplest root to mount for a
smoke test or a generated overlay (e.g. a load order's `Plugins.txt`). Reads
run through libfuse's multithreaded loop; `aether.vfs.fuse` bounds concurrent
in-flight reads (see env vars below) and isolates a bad request to an errno
instead of tearing down the mount. To route more than one provider under a
single mountpoint, build a `aether.vfs.router/router` and mount it with
`fuse/mount-router` instead of `fuse/mount`.

## Composition

`aether.vfs.compose` wires the providers together into the shapes a game
mount actually needs: `aether.vfs.router` dispatches virtual paths to
providers by glob pattern (first match wins, else a default); `providers.layered`
stacks two read-only providers with top-wins precedence, so a mod's files
shadow same-path base-game files while everything else falls through; and
`providers.overlay` sits on top as copy-on-write — it serves reads merged
upper-over-base, records deletes as `.wh.*` whiteout markers, copies a file
up to the writable directory the first time it's modified, and never mutates
the base. `compose/build-data-root`, `build-data-root-over`, and
`build-inline-root` assemble the common combinations (snapshot, snapshot-over-base,
and inline-over-overrides) so callers don't hand-stack layered/overlay themselves.

## Proton example

```clojure
(require '[aether.vfs.proton :as proton])

(let [params {:proton      "/path/to/GE-Proton10-34/proton"
              :mountpoint  "/tmp/my-mount"
              :exe         "Game.exe"
              :steam-root  (proton/default-steam-root)
              :app-id      489830
              :compat      "/tmp/throwaway-compat"}
      cmd  (proton/proton-command params)
      proc (proton/launch-proton! cmd {:logdir "/tmp/throwaway-compat/logs"})]
  (.waitFor proc)
  (proton/teardown! params))
```

`proton-command` builds the `proton run <mountpoint>/<exe>` invocation —
`:compat` must be a throwaway `STEAM_COMPAT_DATA_PATH`, never the caller's
real Steam prefix. `launch-proton!` spawns it as a tracked `Process` with
`PROTON_LOG` on and stdout/stderr captured to `<logdir>/run.log` (Wine's own
trace logging is silenced so the log doesn't grow to gigabytes in seconds).
`proton/drm-failures` and `proton/reached-markers` grep that log directory
for known failure/success signatures. `teardown!` kills only the exe running
at this mount and this run's `wineserver` — never another game's — since the
Wine process tree outlives the `proton` wrapper and keeps hammering the mount
if left running.

## Env vars

- `AETHER_VFS_NO_READ_INTO` — when set, forces the heap `read-at` path even
  if FFM zero-copy reads are available, skipping the `MemorySegment`-backed
  `read-into!` fast path entirely. Useful for isolating whether a bug is in
  the zero-copy path.
- `AETHER_VFS_MAX_FUSE_READS` — caps the number of concurrent kernel read
  requests in flight (default `32`). libfuse's multithreaded loop spawns a
  worker per in-flight request; a slow provider lets them pile up faster than
  they drain, and the accumulated heap-heavy state can exhaust the JVM heap.
  Lower this if reads are large or the backing provider is slow.
- `AETHER_VFS_PROTON_PATH` — overrides the default Proton binary path
  (`<steam-root>/compatibilitytools.d/GE-Proton10-34/proton`) used by
  `proton/default-proton-path`.

## Tests

```
clojure -M:test
```

The FFM zero-copy read path uses restricted methods, so the `:test` alias
runs with `--enable-native-access=ALL-UNNAMED`. `aether.vfs.mount-test`
mounts a real FUSE filesystem in-process and reads a file back through the
kernel — it needs `/dev/fuse` and prints `skip: /dev/fuse not available` and
short-circuits if the device isn't present (e.g. no `fuse` kernel module, or
inside a container without `--device /dev/fuse`).
