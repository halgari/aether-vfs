# aether-vfs — Registry overlay: per-profile copy-on-write of registry keys

Date: 2026-10-05
Status: implemented on the `registry-overlay` branch. This document describes what was built; the
rulings made during implementation are recorded in the plan ledger
(`.superpowers/sdd/2026-10-05-registry-overlay/progress.md`).

## 1. Purpose

Processes the shim injects (the game and the children it follows) must see the real registry,
but nothing they write may reach it. Their writes go to a **registry layer** that belongs to the
session's profile and persists between runs, like the file write layers. One profile's registry
writes never leak into another profile, the shared Wine prefix or the Windows user's registry.

Decisions made with the owner:

- **Isolation per profile**, persistent between runs (not throwaway per session).
- **All of HKCU and HKLM** go through the layer.
- **Only shim-injected processes** see the layer. Wine's services and other processes keep using
  the real registry.
- **Cross-platform:** built on the NT registry API only, so it works the same on Windows, macOS
  and Wine. Nothing Proton-specific.
- **The overlay lives in the director** and is stored in the host's store (not shadow keys in the
  real registry), so it survives prefix rebuilds.
- **No host-side tooling** (inspect, export, reset) and **no injected keys** in this version.

## 2. Architecture

- **Director (host):** owns each session's registry layer: an in-memory key tree loaded from the
  profile's layer at session start, changed only through the ring operations below, and saved per
  the fsync policy. All injected processes of a session talk to the same director, so they share
  one consistent overlay.
- **Shim (in each injected process):** hooks the NT registry calls. It reads the real registry
  through the original (unhooked) entry points, reads the overlay from the director, and presents
  the merged view. Every write by an injected process goes to the overlay; none reaches the real
  registry.

### 2.1 Data model

The overlay is a tree of key nodes. Each node holds:

- **values:** name → (type, data). Names are matched case-insensitively as Windows does, and keep
  the case they were written with;
- **value tombstones:** names deleted here that hide values of the real key;
- **child entries:** names of subkeys present in the overlay, each either *present* (a node) or
  *tombstoned* (hides the real subkey and everything below it);
- **origin:** *created here* (no real counterpart required) or *overlays a real key*;
- **last-write time:** reported by `NtQueryKey`, set by every overlay write to the key.
  `NtSetInformationKey` on a virtualised key succeeds and changes nothing (no ring operation
  carries a last-write time);
- **volatile:** volatile keys and their subtrees are kept in memory only (never saved), as on
  Windows.

### 2.2 Paths

Paths are canonical NT paths, compared case-insensitively:

- `\Registry\Machine\…` for HKLM;
- `\Registry\User\<CurrentUser>\…` for the current user's hive (HKCU). The shim replaces the
  current user's SID with the fixed token `<CurrentUser>` (and `<CurrentUser>_Classes` for the
  user's classes hive), so a profile's layer applies on another machine or Windows account.
  Other users' SIDs are stored as they are;
- HKCR is not a hive of its own: the shim stores writes at the HKLM or HKCU location they resolve
  to;
- WOW64 redirection (`Wow6432Node`) is not interpreted: paths are stored exactly as the process
  resolved them, so a 32-bit process and a 64-bit process see what they would see natively.

## 3. Shim

### 3.1 Hooked calls

- **Open and create:** `NtOpenKey`, `NtOpenKeyEx`, `NtCreateKey`. With the overlay on, the real
  `NtCreateKey` is never called: a key that exists for real is opened (`NtOpenKeyEx`, reported as
  `REG_OPENED_EXISTING_KEY`), and a missing one is created in the overlay. `TitleIndex` and
  `Class` are ignored for overlay keys.
- **Query:** `NtQueryKey`, `NtEnumerateKey`, `NtQueryValueKey`, `NtEnumerateValueKey`,
  `NtQueryMultipleValueKey`.
- **Write:** `NtSetValueKey`, `NtDeleteValueKey`, `NtDeleteKey`, `NtRenameKey`,
  `NtSetInformationKey` (last-write time), `NtFlushKey` (no-op for virtual keys).
- **Notification:** `NtNotifyChangeKey`, `NtNotifyChangeMultipleKeys`.
- **Handles:** `NtClose` and `NtQueryObject` (already hooked) learn about key handles;
  `NtDuplicateObject` is hooked so duplicates of tracked key handles stay tracked;
  `NtSetInformationObject` keeps a synthetic key's handle flags.
- **Security and out of scope:** `NtQuerySecurityObject`, `NtSetSecurityObject`, and the calls of
  section 3.6.

The registry detours are installed only when the host turned the overlay on (`VFS_REGISTRY`, see
section 5). With it off, a process carries none of them; `NtClose` and `NtQueryObject` are the
file hooks' and then never touch the registry tables.

### 3.2 Handle model

On every open or create, the shim resolves the full canonical path: the root handle's path (from
its handle table, or by asking the real key for its name) plus the relative name.

- **Pass-through (fast path):** if the overlay has nothing at or below the path, the caller gets
  the real handle unchanged. The shim records only its path in a handle table, so writes through
  it can be intercepted. A read through it costs one `REG_LOOKUP`, answered from the shim's cache
  (section 3.5); after any registry write in the session, the first such read is a ring round
  trip.
- **Virtual:** if the overlay touches the path or anything below it, if the key exists only in the
  overlay, or if its real counterpart is tombstoned, the caller gets a synthetic handle (like the
  shim's synthetic file handles). Every query on it is answered by merging the real key (opened
  privately through the unhooked call, when it exists) with the overlay node. A node *created
  here* hides the real key's contents: such nodes exist only for keys deleted then recreated, or
  with no real counterpart, so real contents showing through would resurrect deleted data.
- **Copy-on-write on a pass-through handle:** a write through a real handle goes to the director,
  never to the real key. From then on the path counts as virtual, and later queries on the same
  handle are routed through the merge using the handle table. The caller keeps its handle.
- **Desired access:** a write through a handle opened without write access fails with
  `STATUS_ACCESS_DENIED`, as on Windows. Write access is granted for virtual keys even where the
  real key would refuse it, because the write never reaches the real key: if opening the real key
  fails with `STATUS_ACCESS_DENIED` and the requested access includes write rights, the shim opens
  the real key read-only and returns a virtual handle with the requested access. On Windows,
  injected processes therefore need no administrator rights to change HKLM.
- **Handles opened elsewhere:** a key handle the tables do not hold (opened before the hooks, or
  handed in) is resolved on first sight from the real key's name. Only "not a key" or "not a
  handle" (`STATUS_OBJECT_TYPE_MISMATCH`, `STATUS_INVALID_HANDLE`) makes it *not ours*; any other
  failure leaves it *unresolvable* (section 6).
- **All or nothing per process:** registry virtualisation is on in a process only if every
  registry detour was installed. If any is missing, every registry hook passes through for the
  life of the process (logged, and flagged in the shim stats): a missing write hook would
  otherwise let writes through a virtual view reach the real registry.

### 3.3 Merge rules

- **Values:** overlay values first, then the real key's values, minus tombstoned names.
- **Subkeys:** the real key's subkeys in their real order, minus tombstoned names, then
  overlay-created subkeys sorted case-insensitively. The order is stable across calls, so index-
  based enumeration (`NtEnumerateKey` with an increasing index) sees each key exactly once.
- **Counts and sizes** in `KEY_FULL_INFORMATION` / `KEY_CACHED_INFORMATION` (subkey count, value
  count, longest names, longest data) are computed over the merged view.
- **Layouts:** query results use the exact NT layout of every information class, including
  alignment and the `STATUS_BUFFER_OVERFLOW` (partial data) versus `STATUS_BUFFER_TOO_SMALL` (no
  data) distinction, and the required length in `ResultLength`. Where Wine and Windows x64
  differ, the Windows layout is used, with one exception: an empty value's `DataOffset` is the
  end of its name, as Wine reports it, not `0xFFFFFFFF` (Wine's `RegEnumValueW` derives a length
  from the offset; Windows reads `DataLength` 0 and never uses it).
- **Information classes:**
  - `NtQueryKey`: Basic, Node, Full, Name, Cached, Flags, Virtualization, HandleTags,
    Trust (where the platform supports it; otherwise pass the real call through);
  - `NtEnumerateKey`: Basic, Node, Full;
  - `NtQueryValueKey` and `NtEnumerateValueKey`: Basic, Full, Partial, FullAlign64,
    PartialAlign64.

### 3.4 Notifications

`NtNotifyChangeKey` / `NtNotifyChangeMultipleKeys` on any key on a virtualised path (synthetic or
pass-through) wait on the overlay only: they complete when the overlay changes at the key (or
below it, with `WatchTree`), which the shim learns by polling `REG_CHANGED` every 250 ms. Changes
made to the real registry by other processes are not watched on virtualised paths in this
version; nothing else writes those keys during a game. Keys outside HKLM and HKU get the real
call.

- `NtNotifyChangeMultipleKeys` with subordinate keys (`Count` > 0) on a served key returns
  `STATUS_NOT_SUPPORTED`.
- An asynchronous notify with an event and no APC gets no write to its `IO_STATUS_BLOCK` (Wine's
  kernelbase passes a stack block for that form).
- Closing a key ends its pending notifies with `STATUS_NOTIFY_CLEANUP`.

### 3.5 Caching

The shim caches `REG_LOOKUP` and `REG_KEY` results per path, tagged with the director's registry
generation. The director publishes the generation in the header of every ring of the session
(`reg_gen`; ring `VERSION` 4) and moves it on every registry write, before the writer gets its
reply, and on every attach or detach of a layer. An answer is tagged with the generation read
before asking, and used only while the published generation still equals it, so no injected
process uses a stale entry. A cache hit costs one atomic load and no ring round trip; a miss costs
one.

### 3.6 Out of scope

The calls that would change the real registry — `NtRestoreKey`, `NtReplaceKey`, `NtLoadKey*`,
`NtUnloadKey*`, and transacted create and open (`NtCreateKeyTransacted`,
`NtOpenKeyTransacted(Ex)`) — return `STATUS_NOT_SUPPORTED` on every virtualised path, even through
pass-through handles. The harmless ones — `NtSaveKey*`, `NtCompressKey`, `NtLockRegistryKey` —
return `STATUS_NOT_SUPPORTED` on synthetic handles and pass through on real ones. An export the
platform lacks is simply not hooked. Key security descriptors are those of the real key, or the
nearest real ancestor's for overlay-created keys: `NtQuerySecurityObject` on a synthetic key
returns that descriptor, and `NtSetSecurityObject` on any key on a virtualised path is checked
(access, descriptor validity), accepted and ignored.

## 4. Ring protocol

All paths are canonical NT paths (section 2.2).

| Op | Request | Reply |
|---|---|---|
| `REG_LOOKUP` | path | node state (absent / present / created here / tombstoned), whether anything exists below, version |
| `REG_KEY` | path | the node's values and value tombstones, child entries (present / tombstoned), origin, last-write time, version |
| `REG_SET_VALUE` | path, name, type, data | version |
| `REG_DELETE_VALUE` | path, name | version |
| `REG_CREATE_KEY` | path, volatile | version; missing ancestor nodes are created |
| `REG_DELETE_KEY` | path | version; tombstones the key and drops its overlay subtree |
| `REG_RENAME_KEY` | path, new leaf name | version; moves overlay nodes only |
| `REG_CHANGED` | path, subtree flag, version | whether the overlay changed under the path after that version, and the current version (answered at once) |

Opcodes 15–22, in this order. Reads are concurrent. Writes are serialised in the director, and
each write bumps a single overlay version.

- **Rename.** `REG_RENAME_KEY` moves overlay nodes only. The shim renames a key with real content
  by a bounded copy of the merged subtree to created-here nodes, then tombstones the source (not
  atomic). Other handles on the old key, and handles on its subkeys, then get
  `STATUS_KEY_DELETED`.
- **Delete.** `NtDeleteKey` of a key whose merged view has subkeys returns
  `STATUS_CANNOT_DELETE`.
- **Statuses.** Besides the shared ring statuses, a `REG_KEY` reply larger than the ring payload
  gets "reply too large" (-11); the shim treats it as a director read failure (section 6).

## 5. Director and storage

- **In memory:** the session's tree.
- **Saves:** the whole non-volatile tree is written as one file (`overlay.reg.tmp`, renamed over
  `overlay.reg`), at most once a second while it is dirty, and at every flush (session stop,
  detach or replacement of the layer, and the host's drop). Registry layers are small, so
  whole-file saves are simpler than logs.
- **Durability:** the fsync policy: deferred, with a durable point at session end and at least
  every few minutes. A save is an ordinary write to the layer, which a deferred store does not
  make durable by itself, so the host passes its store's sync as the layer's sync hook
  (`Session::set_registry_layer(layer, Some(sync))`; `registry_sync_for(&storage)` for a
  `Storage`). The session calls it after the final save when it stops serving and when the layer
  is detached or replaced; while a save is not yet covered by it, the saver calls it at most once
  every five minutes. Without a hook, durability is the store's own policy.
- **Format:** a small versioned binary encoding of the tree. Values keep the types they were
  written with (`REG_SZ`, `REG_EXPAND_SZ`, `REG_BINARY`, `REG_DWORD`, `REG_QWORD`,
  `REG_MULTI_SZ`, and any other type number as raw bytes), so later tooling can export `.reg`
  files.
- **Location:** a file in the session's storage, in the profile's write-layer area. A new session
  setting names it: the registry layer of the session. While a layer is attached, launches set
  `VFS_REGISTRY=1`; without it, registry virtualisation is off and the registry detours are not
  installed. Attaching and detaching take effect for new launches only: a process running when
  the layer is detached keeps its hooks, its registry writes fail and its overlay disappears, so
  hosts change the layer only between launches.
- **Limits** (as Windows): key names up to 255 characters, value names up to 16,383 characters,
  values up to 1 MiB in this version, less the request's path and name overhead, because a
  request must fit one ring payload. Larger requests fail with `STATUS_INVALID_PARAMETER`.

## 6. Errors and safety

- **Writes never fall back to the real registry.** If the director is unreachable or a write
  fails, the call returns `STATUS_UNSUCCESSFUL`. So does a write through a key handle whose real
  name cannot be read (an *unresolvable* handle, section 3.2: it may be on a virtualised path), and
  a real-modifying call (a write, `NtSetSecurityObject`, or a section 3.6 call that changes the
  real registry) made while the hook is bypassed (the shim's own work) with the overlay on.
- **All or nothing per process** (section 3.2): a process missing any registry detour does not
  virtualise its registry at all.
- **Reads fall back to the real registry only.** If the director is unreachable, the shim serves
  pass-through, so a broken director never blocks a game from starting. A read through an
  unresolvable handle passes through too (it is not remembered as not ours). The shim stats count
  every fallback.
- **Hook panics** return `STATUS_HOOK_PANICKED`, as the file hooks do.
- **Shim stats:** per-call registry counts and times, and the number of virtual versus
  pass-through key handles.

## 7. Haskill

- Each list and profile gets a registry layer next to its file write layers in the store.
  Haskill sets the session's registry layer when it builds the session.
- No commands or options in this version.

## 8. Testing

1. **Director (Linux):** unit tests for the tree, tombstones, case-insensitive matching, the
   version and change waits, limits, volatile keys, save/load round trips, and a crash between
   durable points (the layer is the last durable state).
2. **Shim logic (Linux):** the merge and NT layout code as pure Rust functions, tested against
   hand-built expected structures for every information class, including partial-buffer cases.
3. **End to end (Wine):** a Windows test program, built with the cross toolchain and run under
   Wine, runs the same scripted sequence (open, create, set, query in every class, enumerate,
   delete, rename, notify) twice: on a scratch real key with the shim off, and on the same key
   through the overlay with the shim on. The results must match byte for byte, and the real
   registry must be unchanged after the overlaid run.
4. **In game:** a Journals of Jyggalag launch. The shim stats show the game's registry writes
   going to the overlay, and the prefix's registry files are unchanged afterwards.
