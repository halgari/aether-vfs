# M5 — Unified Entry + Packaging + Docs (design)

**Status:** approved (brainstorm 2026-07-30). Milestone M5 of the unified
cross-platform VFS (see `2026-07-26-unified-cross-platform-vfs-design.md`).

## Goal

Deliver the single "import one library and it works on both OSes" entry point
the whole merge was aiming at: one public namespace, `aether.vfs`, exposing a
verb **`run`** that executes a target program inside a `Provider`-backed virtual
filesystem, scoped to that program's lifetime, dispatching on the host OS. Plus
the packaging (a tools.build jar that bundles the Windows native artifacts) and
docs that make it consumable.

## Decisions (from brainstorm)

1. **Unify on `run` (process-scoped, both OSes).** Not ambient `mount`. `run`
   starts the VFS, runs the target, and tears the VFS down when the target
   exits — the game-modding model (USVFS/MO2-style). Windows = injected shim;
   Linux = FUSE mount → spawn target from the mountpoint → wait → unmount.
2. **Native artifact resolution = override + bundled fallback.** An explicit
   `:native-dir` opt / `AETHER_VFS_NATIVE_DIR` env wins; otherwise extract the
   three Windows artifacts bundled as classpath resources to a cache dir.
3. **Packaging = build pipeline, publish later.** A tools.build `build.clj`
   builds the Rust artifacts (`--release`), stages them into
   `resources/native/windows/`, and produces an importable jar. Clojars/Maven
   publishing is out of scope.
4. **Linux launch matches the existing no-admin code.** Generalize
   `os/linux/proton.clj`'s pattern: mount at a mountpoint, run the target with
   `:cwd` = mountpoint (so its file access resolves through the VFS), wait,
   then teardown (kill by mount path) + unmount. User-space FUSE only, **no
   admin / no mount namespaces / no bind-mounts.**

## Architecture

```
            (require '[aether.vfs :as vfs])
                       vfs/run  ──dispatch on OS──┐
                          │                       │
        ┌─────────────────┘                       └──────────────────┐
   os/windows/launch/launch                    os/linux/launch/run
   (existing injected shim)                    (NEW: mount+spawn+teardown,
        │                                        generalized from proton.clj)
   os/windows/artifacts/resolve!              os/linux/fuse/mount-router
   (NEW: :native-dir/env override →           (existing FUSE)
    else extract resources → cache)
```

`aether.vfs` is a new top-level namespace file `src/aether/vfs.clj` (coexists
with the `aether.vfs.*` package). It does OS detection, opts normalization, and
delegates. The ambient `os/linux/fuse/mount` and the Proton launcher stay as-is
for their specialized uses; `run` is the unified verb.

## Public API

```clojure
(vfs/run provider opts) ; => ^long target exit code

;; opts (unified; unknown OS-specific keys pass through OS escape hatches):
;;   :exec        [cmd & args]  REQUIRED. Target program + args to run in the VFS.
;;   :mountpoint  String        Linux: dir to mount at (default: a fresh temp dir,
;;                              created + removed by run). Windows: the virtual
;;                              root (default "C:\\GameLayers\\runtime").
;;   :native-dir  String        Windows only: dir holding injector/shim/payload;
;;                              overrides the bundled resources. (env
;;                              AETHER_VFS_NATIVE_DIR is the lower-priority override.)
;;   :env         {k v}         Extra environment for the target process.
;;   :windows     {..}          Escape hatch: merged into the Windows launch opts
;;                              (:payload-cap :slot-count :arena-len etc.).
;;   :linux       {..}          Escape hatch: merged into the Linux launch opts.
```

`run` throws `ex-info` with a clear message on an unsupported OS (anything not
Windows or Linux).

### Windows mapping
`run` builds the existing `os/windows/launch/launch` opts:
`:target-exe` = `(first exec)`, `:target-args` = `(rest exec)`,
`:child-env` = `:env`, `:root` = `:mountpoint` (default kept),
plus the resolved `:injector`/`:shim-dll`/`:payload` from
`os/windows/artifacts/resolve!`, plus any `:windows` passthrough. Returns the
launch exit code.

### Linux mapping
`os/linux/launch/run`:
1. Resolve mountpoint (opt or a fresh temp dir it will create).
2. `mount-router` (or `mount` for a bare provider) at the mountpoint.
3. Spawn `:exec` via `ProcessBuilder`, `:cwd` = mountpoint, env += `:env`
   (and `AETHER_VFS_MOUNT` = mountpoint so the target can find it), inheritIO.
4. `.waitFor` → exit code.
5. `finally`: teardown — best-effort kill of the process tree, then unmount
   (close the mount guard), then remove a temp mountpoint we created.

## Native artifact resolution (`os/windows/artifacts.clj`)

```clojure
(resolve! opts) ; => {:injector path :shim-dll path :payload path}
```
Resolution order per artifact set:
1. `:native-dir` opt (if present) — expect `vfs-injector.exe`,
   `vfs_shim_dll.dll`, `vfs_payload.dll` inside it.
2. `AETHER_VFS_NATIVE_DIR` env — same layout.
3. Bundled: classpath resources `native/windows/<name>`, extracted to a cache
   dir (`<tmp>/aether-vfs-native/<version-or-hash>/`) once, reused thereafter
   (skip re-extract if present and same size). Extracted `.exe`/`.dll` marked
   executable/readable.
Missing at every tier → `ex-info` naming the artifact and the tiers tried.

## Packaging (`build.clj`, tools.build)

- `:build` alias in `deps.edn` adding `io.github.clojure/tools.build`.
- `stage-native` task: `cargo build --release -p vfs-inject -p vfs-shim-dll
  -p vfs-payload` then copy the three artifacts from `rust/target/release/`
  into `resources/native/windows/`.
- `jar` task: `stage-native` (on Windows/when artifacts exist) → `write-pom` →
  `compile`-free (Clojure source jar) → `jar` including `resources` (hence the
  staged natives). Coordinates `com.halgari/aether-vfs`, version from a `VERSION`
  or git.
- `resources/native/windows/` is **gitignored** (no binaries in git); the jar
  and CI staging populate it.

## CI

Extend `.github/workflows/ci.yml`:
- **windows-clojure** job: after building the Rust debug artifacts, add a
  step that stages them into `resources/native/windows/` (copy, since release
  isn't built in CI), so a new **bundled-path** launch test runs `vfs/run`
  WITHOUT `:native-dir` and proves resource→cache resolution + injection.
  The existing explicit-path e2e migrates to `vfs/run` with `:native-dir`.
- **ubuntu** job: add a `vfs/run` Linux e2e — mount an inline provider, run a
  shell command reading `<mount>/hello.txt`, assert output + exit 0.

## Testing

- **artifacts_test** (cross-platform, pure): `:native-dir` override returns its
  paths; a fake resource extracted to a temp cache dir round-trips (size-skip on
  second call); all-missing → `ex-info`. Uses a temp classpath/URL or a direct
  extraction fn seam so it runs on Linux CI too.
- **vfs (dispatch) test** (cross-platform): OS-dispatch picks the right adapter
  via an injectable seam (mock launch fns); unsupported OS → `ex-info`; opts
  normalization (exec→target-exe/args, env, mountpoint default) asserted.
- **linux/launch test** (Linux CI): `run` an inline provider + `sh -c 'cat
  $AETHER_VFS_MOUNT/hello.txt'`, assert stdout "hello" and exit 0; mountpoint
  removed after.
- **windows launch-test** (Windows CI): migrate `injected-*` tests to `vfs/run`;
  add one bundled-path case (staged resources, no `:native-dir`).

## File structure

- Create `src/aether/vfs.clj` — unified `run` + OS dispatch + opts normalize.
- Create `src/aether/vfs/os/linux/launch.clj` — `run` (mount+spawn+teardown).
- Create `src/aether/vfs/os/windows/artifacts.clj` — `resolve!`.
- Modify `src/aether/vfs/os/windows/launch.clj` — accept resolved artifacts
  (no behavior change; keep taking explicit paths, `run` supplies them).
- Create `build.clj` + `:build` alias in `deps.edn`.
- Modify `.gitignore` — add `resources/native/`.
- Modify `.github/workflows/ci.yml` — stage step + Linux/bundled e2e.
- Create/modify tests as above.
- Docs: a README section (cross-platform `run` usage, per-OS behavior, artifact
  resolution, no-admin note, build instructions).

## Out of scope (deferred)

Clojars/Maven publish; bind-mount / mount-namespace redirection on Linux;
folding the Proton launcher into `run`; open-existing-writable (M4 carryover)
and the other tracked follow-ups.

## Self-review

Coverage: unified `run` + OS dispatch (aether.vfs), Linux launch glue
(os/linux/launch), Windows artifact resolution (os/windows/artifacts),
packaging (build.clj), CI e2e both OSes, docs. Decisions 1–4 each map to a
component. No admin/namespace work (matches existing code). Binaries stay out
of git (gitignored stage dir; jar/CI populate). Windows launch stays
backward-compatible (explicit paths still work; `run` supplies resolved ones).
```
