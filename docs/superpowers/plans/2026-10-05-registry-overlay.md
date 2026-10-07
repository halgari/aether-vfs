# Registry overlay — implementation plan

**Status:** executed and merged; kept as the reference for how it was built. The spec is `specs/2026-10-05-registry-overlay-design.md`; what is durable when is in `rust/docs/durability.md`.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Processes the shim injects see the real Windows registry merged with a per-profile
copy-on-write overlay that the director holds and the host persists; none of their registry
writes reach the real registry.

**Architecture:**
- **Portable core:** a new crate `vfs-registry`, built and tested on Linux. It holds the
  overlay model, persistence format, path canonicalisation, the merge of a real key with an
  overlay node, and the NT layouts of every key and value information class.
- **Ring protocol:** new opcodes in `vfs-protocol`.
- **Director:** a `RegistryHost` inside `vfs-director` owns the overlay and saves it through a
  provider.
- **Host side:** `vfs-embed` lets the host attach a registry layer and turns the shim's hooks on.
- **Shim:** `vfs-shim` hooks the NT registry calls, and keeps a key-handle table plus synthetic
  key handles.
- **Haskill:** only attaches a per-profile registry layer.

**Tech stack:** Rust (edition 2021 in aether-vfs); `windows-sys` and `retour` in the shim;
`cargo xwin` (clang-cl + xwin SDK) for Windows builds; Wine (GE-Proton) for the end-to-end test.

**Spec:** `docs/superpowers/specs/2026-10-05-registry-overlay-design.md` (aether-vfs).
**Code map** (where things are, with file:line):
`/home/tbaldrid/oss/haskill/.superpowers/stream/registry-codemap.md`. Read it before any task.

## Global Constraints

- Nothing Proton-specific in the registry code. The NT registry API only, so it is identical on
  Windows, macOS and Wine.
- A write by an injected process never reaches the real registry. If the overlay cannot take a
  write, the call fails (`STATUS_UNSUCCESSFUL`).
- Without a registry layer attached to the session, behaviour is exactly as today: the registry
  hooks are not installed.
- Most of the code lives in aether-vfs. Haskill's part is limited to creating the layer and
  attaching it.
- Every hook goes through `hook_entry_points!` (enforced by the test
  `no_extern_hook_bypasses_the_panic_containment_macro`), and every new hook gets a `hookstats`
  entry.
- New ring opcodes are 15–22. No ring `VERSION` bump: these are new opcodes, not changed layouts.
  Regenerate `resources/protocol-descriptor.edn` with `bin/regen-protocol`.
- Limits as Windows: key name ≤ 255 UTF-16 units, value name ≤ 16,383, value data ≤ 1 MiB
  (`STATUS_INVALID_PARAMETER` above).
- Builds and scratch files never go under `/tmp`.
- Commits end with `Claude-Session: https://claude.ai/code/session_01LpMJHm1UFrXPeWreERuLqY`.

## Rulings (recorded against the spec)

- **R1: no blocking wait.** Spec §4 has `REG_WAIT_CHANGE` blocking until a change. A blocking
  request would hold a director ring worker for a long time, and the ring times a request out
  after 60 s. So opcode 22 is `REG_CHANGED`: it answers at once whether anything at or under a
  path changed after a given version, and the current version. The shim's notification thread
  polls it every 250 ms, and only while a notification is pending. The observable behaviour is
  the one in spec §3.4. If this is wrong, notifications fire up to 250 ms late.
- **R2: save after each write.** The director saves the whole non-volatile overlay through the
  layer provider after each write, debounced to at most once per second, and on
  `RegistryHost::flush` (session end). Durability then follows the storage's deferred policy and
  the host's sync at close, which is the fsync policy. If this is wrong, a crash loses writes
  made since the last durable point, as with file layers.
- **R3: one file per layer.** The registry layer is a storage layer holding one file,
  `overlay.reg`. Haskill names it `haskill.{list}.{profile}.registry`. No root is composed over
  it, so the game never sees the file.

## Review Focus

1. **Index-based enumeration of a merged key while it changes.** A game enumerates with
   `NtEnumerateKey(i)` and deletes keys during the loop. Expected: each surviving key is seen
   exactly once and no index is skipped, matching Windows for real keys. Test in Task 3.
2. **Partial buffers.** `NtQueryValueKey` with a buffer that holds the header but not the data.
   Expected: `STATUS_BUFFER_OVERFLOW`, the header filled, and `ResultLength` set to the full
   size. A buffer smaller than the fixed header gives `STATUS_BUFFER_TOO_SMALL`. Test in Task 4.
3. **Copy-on-write through an existing real handle.** The game opens a key read-write (real
   handle), sets a value, then queries it through the same handle. Expected: the new value, with
   the real registry unchanged. Test in Task 11 and the end-to-end test in Task 13.
4. **A director that fails.** Reads must still work (pass-through) and writes must fail cleanly.
   Test in Task 8.
5. **HKCU portability.** A layer written under one user's SID applies under another SID. Test in
   Task 1.

---

### Task 1: `vfs-registry` crate — paths and the overlay model

**Files:**
- Create `rust/crates/vfs-registry/{Cargo.toml,src/lib.rs,src/path.rs,src/overlay.rs}`.
- Modify `rust/Cargo.toml` (workspace member) and `.github/workflows/ci.yml` (add to the native
  Linux test list next to `vfs-core`).

**Interfaces (produces):**
```rust
// path.rs
pub const CURRENT_USER: &str = "<CurrentUser>";
/// Canonical form of an NT key path: `\Registry\...`, single separators, no trailing `\`.
/// The current user's SID (`user_sid`) and `user_sid + "_Classes"` are replaced by
/// CURRENT_USER and CURRENT_USER + "_Classes". Comparison stays case-insensitive (fold()).
pub fn canonical(nt_path: &str, user_sid: Option<&str>) -> Result<String, PathError>;
/// The reverse, for building names to give back to the process (NtQueryKey KeyNameInformation).
pub fn to_nt(canonical: &str, user_sid: Option<&str>) -> String;
pub fn fold(s: &str) -> String;            // case fold used for lookups (vfs_core::fold)
pub fn parent(canonical: &str) -> Option<&str>;
pub fn leaf(canonical: &str) -> &str;
pub fn join(base: &str, rel: &str) -> Result<String, PathError>;   // rejects "..", empty parts
pub fn is_virtualised(canonical: &str) -> bool; // under \Registry\Machine or \Registry\User

// overlay.rs
pub struct Value { pub name: String, pub ty: u32, pub data: Vec<u8> }
pub enum Child { Present, Tombstone }
pub struct Node {
    pub values: Vec<Value>,               // in insertion order; lookups by fold(name)
    pub value_tombstones: Vec<String>,    // folded names deleted here
    pub children: BTreeMap<String /*folded*/, (String /*stored spelling*/, Child)>,
    pub created: bool,                    // created here (no real counterpart required)
    pub volatile: bool,
    pub last_write: u64,                  // FILETIME
}
pub struct Overlay { /* tree keyed by canonical path, version: u64 */ }
pub enum Lookup { Absent, Present { created: bool }, Tombstoned }
impl Overlay {
    pub fn new() -> Self;
    pub fn version(&self) -> u64;
    pub fn lookup(&self, path: &str) -> (Lookup, bool /*anything below*/);
    pub fn node(&self, path: &str) -> Option<&Node>;
    pub fn set_value(&mut self, path: &str, name: &str, ty: u32, data: &[u8], now: u64) -> Result<u64, RegError>;
    pub fn delete_value(&mut self, path: &str, name: &str, now: u64) -> Result<u64, RegError>;
    pub fn create_key(&mut self, path: &str, volatile: bool, real_exists: bool, now: u64) -> Result<u64, RegError>;
    pub fn delete_key(&mut self, path: &str, now: u64) -> Result<u64, RegError>; // tombstone + drop subtree
    pub fn rename_key(&mut self, path: &str, new_leaf: &str, now: u64) -> Result<u64, RegError>;
    pub fn changed_since(&self, path: &str, subtree: bool, version: u64) -> bool;
}
pub enum RegError { NameTooLong, DataTooLarge, NotFound, AlreadyExists, InvalidPath }
```
Every mutation bumps the version and records the change for `changed_since`. Missing ancestors
are created as present (not created-here) nodes. Writing under a tombstoned path revives the
chain: the tombstone is replaced by a created-here node.

- [ ] Write tests:
  - canonicalisation: `\REGISTRY\MACHINE\Software\\X\` → `\Registry\Machine\Software\X`;
  - SID replacement, both directions, including `_Classes`;
  - `join` rejecting `..`;
  - case-insensitive lookup with spelling preserved;
  - set, then delete, then set the same value;
  - delete a key with a subtree, then lookup gives `Tombstoned` and `node()` gives `None` for
    descendants;
  - revive under a tombstone;
  - limits (255/16383/1 MiB);
  - the version bumps once per mutation;
  - `changed_since` for subtree and non-subtree;
  - Review Focus 5 (a layer written with SID A, read with SID B).
- [ ] Run `cargo test -p vfs-registry` and see the tests fail. Implement until they pass.
- [ ] Commit.

### Task 2: `vfs-registry` — persistence format

**Files:** create `rust/crates/vfs-registry/src/format.rs`.

**Interfaces:**
```rust
pub const MAGIC: &[u8; 8] = b"AEREG\0\0\x01";
pub fn encode(o: &Overlay) -> Vec<u8>;            // excludes volatile nodes and their subtrees
pub fn decode(b: &[u8]) -> Result<Overlay, FormatError>; // version restarts at 1
```
Layout: a little-endian, length-prefixed, depth-first walk. Each node has its stored spelling,
flags, last-write time, values (name, type, data), value tombstones, and child count. A trailing
xxhash-free checksum (a sum of u64 words) lets corruption be detected.

- [ ] Tests:
  - round trip of a tree with every value type and with tombstones;
  - volatile nodes are not saved;
  - a truncated or corrupt file gives `FormatError`;
  - an empty overlay round-trips;
  - a format-version mismatch is refused.
- [ ] Implement, run, commit.

### Task 3: `vfs-registry` — the merged view

**Files:** create `rust/crates/vfs-registry/src/merge.rs`.

**Interfaces:**
```rust
/// What the shim read from the real key (through the unhooked calls). None if it doesn't exist.
pub struct RealKey { pub subkeys: Vec<String>, pub values: Vec<Value>, pub class: Option<Vec<u16>>, pub last_write: u64 }
pub struct MergedKey { pub subkeys: Vec<String>, pub values: Vec<Value>, pub class: Option<Vec<u16>>, pub last_write: u64 }
pub fn merge(real: Option<&RealKey>, node: Option<&Node>, tombstoned: bool) -> Option<MergedKey>;
```
Rules (spec §3.3):
- **Values:** overlay values, then real values minus overlay names and tombstones.
- **Subkeys:** real subkeys in their order minus tombstones, then overlay-created children sorted
  case-insensitively.
- **Last write:** the later of the real and overlay times.
- **Tombstoned or both absent:** `None`.

- [ ] Tests:
  - every rule above;
  - a stable order across calls;
  - Review Focus 1: enumerate by index while deleting keys through the overlay; each survivor is
    seen exactly once, as a re-enumeration on Windows would see it.
- [ ] Implement, run, commit.

### Task 4: `vfs-registry` — NT layouts

**Files:** create `rust/crates/vfs-registry/src/layout.rs`.

**Interfaces:**
```rust
pub enum KeyInfoClass { Basic=0, Node=1, Full=2, Name=3, Cached=4, Flags=5, Virtualization=6, HandleTags=7 }
pub enum ValueInfoClass { Basic=0, Full=1, Partial=2, FullAlign64=3, PartialAlign64=4 }
pub struct Written { pub status: i32, pub result_length: u32 }
pub fn write_key_info(class: KeyInfoClass, key: &MergedKey, name_for_name_class: &str, buf: &mut [u8]) -> Written;
pub fn write_subkey_info(class: KeyInfoClass /*Basic|Node|Full*/, name: &str, sub: &MergedKey, buf: &mut [u8]) -> Written;
pub fn write_value_info(class: ValueInfoClass, v: &Value, buf: &mut [u8]) -> Written;
/// NtQueryMultipleValueKey: fill `entries` (KEY_VALUE_ENTRY {ValueName ptr unchanged, DataLength,
/// DataOffset, Type}) and copy data into `buf` (8-byte aligned offsets relative to buf start).
/// A None in `values` gives STATUS_OBJECT_NAME_NOT_FOUND; a short buf gives
/// STATUS_BUFFER_OVERFLOW with entries filled and result_length = bytes required.
pub struct ValueEntry { pub data_length: u32, pub data_offset: u32, pub ty: u32 }
pub fn write_multiple_values(values: &[Option<&Value>], entries: &mut [ValueEntry], buf: &mut [u8]) -> Written;
```
Status rules:
- `STATUS_BUFFER_TOO_SMALL` (0xC0000023) when the fixed part doesn't fit.
- `STATUS_BUFFER_OVERFLOW` (0x80000005) when the fixed part fits but the rest doesn't. The fixed
  part is written and the length fields hold the full sizes.
- `result_length` is always the full size.
- The Align64 classes align the data offset to 8.

Check every structure's offsets against the Windows SDK headers (`wdm.h`/`ntddk.h` in the xwin
SDK).

- [ ] Tests:
  - exact bytes for each class with known inputs (hand-built expected buffers);
  - the two short-buffer cases for each class (Review Focus 2);
  - `KEY_FULL_INFORMATION` counts and maxima over a merged key;
  - Align64 padding.
- [ ] Implement, run, commit.

### Task 5: protocol opcodes

**Files:**
- Modify `rust/crates/vfs-protocol/src/lib.rs` (constants and codecs).
- Modify `rust/crates/vfs-ipc/src/layout.rs` (the catalog).
- Modify `rust/crates/xtask-descriptor/src/lib.rs`.
- Regenerate `resources/protocol-descriptor.edn` with `bin/regen-protocol`.

**Interfaces:**
```rust
pub const OP_REG_LOOKUP: u32 = 15;  pub const OP_REG_KEY: u32 = 16;
pub const OP_REG_SET_VALUE: u32 = 17; pub const OP_REG_DELETE_VALUE: u32 = 18;
pub const OP_REG_CREATE_KEY: u32 = 19; pub const OP_REG_DELETE_KEY: u32 = 20;
pub const OP_REG_RENAME_KEY: u32 = 21; pub const OP_REG_CHANGED: u32 = 22;
// requests: path (u32 len + UTF-8) then op fields; replies: version u64 first.
pub fn encode_reg_path(path: &str) -> Vec<u8>; pub fn decode_reg_path(b: &[u8]) -> Option<&str>;
pub fn encode_reg_set_value(path, name, ty, data) -> Vec<u8>; /* + decode */
pub fn encode_reg_lookup_reply(state: u8, below: bool, version: u64) -> Vec<u8>; /* + decode */
pub fn encode_reg_key_reply(node: &vfs_registry::Node, version: u64) -> Vec<u8>; /* + decode */
pub fn encode_reg_changed(path, subtree: bool, version: u64) -> Vec<u8>; /* reply: changed u8 + version u64 */
```
`vfs-protocol` depends on `vfs-registry` for `Node`. Alternatively, define a protocol-local
mirror type if a dependency cycle appears. `vfs-registry` must not depend on `vfs-protocol`.

- [ ] Tests: a round trip of every codec; malformed inputs give `None`; the descriptor test
  passes after regenerating.
- [ ] Implement, run `cargo test -p vfs-protocol -p vfs-ipc -p xtask-descriptor`, commit.

### Task 6: director — `RegistryHost` and dispatch

**Files:**
- Create `rust/crates/vfs-director/src/registry.rs`.
- Modify `rust/crates/vfs-director/src/{director.rs,ring_dispatch.rs,lib.rs}`.

**Interfaces:**
```rust
pub struct RegistryHost { /* Mutex<Overlay>, Arc<dyn Provider> store, dirty flag, saver thread */ }
impl RegistryHost {
    /// Loads `overlay.reg` from the provider (absent file = empty overlay; a corrupt file is
    /// renamed to `overlay.reg.corrupt-<unix time>` and an empty overlay is used, logged at error).
    pub fn open(store: Arc<dyn Provider>) -> Result<Arc<Self>, i32>;
    pub fn flush(&self) -> Result<(), i32>;   // save now if dirty (write tmp + rename), wait for it
}
impl Director { pub fn set_registry(&self, host: Option<Arc<RegistryHost>>); pub fn registry(&self) -> Option<Arc<RegistryHost>>; }
```
`dispatch_director` arms for opcodes 15–22:
- No registry attached: `ST_NOT_SUPPORTED`.
- `RegError` mapping: `NameTooLong`/`DataTooLarge`/`InvalidPath` → `ST_BAD_REQUEST`;
  `NotFound` → `ST_NOT_FOUND`.
- The shim maps those to NT statuses.

Saving (R2): a background thread saves at most once per second while dirty, and `flush` saves at
once.

- [ ] Tests (Linux, with an in-memory provider; `vfs-compose` `MemoryProvider` or
  `vfs-provider` `RwMemFixture`):
  - every opcode through `dispatch_director`;
  - persistence across `open` → write → `flush` → reopen;
  - a corrupt file is moved aside;
  - concurrent readers during writes;
  - `REG_CHANGED`;
  - no registry attached → `ST_NOT_SUPPORTED`.
- [ ] Implement, run `cargo test -p vfs-director`, commit.

### Task 7: `vfs-embed` — attach a registry layer; enable the shim

**Files:**
- Modify `rust/crates/vfs-embed/src/session.rs`.
- Modify `rust/crates/vfs-env/src/lib.rs` (a new env name `VFS_REGISTRY`).
- Modify `rust/crates/vfs-proton/src/launch.rs` (pass the flag in `launch_env` and keep it out of
  the scrub list).
- Modify the Windows serve path (`IpcServe::apply_env_roots`) to set the flag.

**Interfaces:**
```rust
impl Session {
    /// Attach (Some) or detach (None) the session's registry layer: a provider holding overlay.reg.
    /// While attached, launches set VFS_REGISTRY=1 so injected processes install the registry hooks.
    pub fn set_registry_layer(&self, layer: Option<Arc<dyn Provider>>) -> Result<(), VfsError>;
}
```
`Session` stop and drop call `RegistryHost::flush`.

- [ ] Tests:
  - attach, then the env carries `VFS_REGISTRY=1`; detach, then it doesn't;
  - stopping the session flushes, and the file is present in the provider;
  - `vfs-env`'s registered-switch test includes the new name.
- [ ] Implement, run `cargo test -p vfs-embed -p vfs-env -p vfs-proton`, commit.

### Task 8: shim — registry client

**Files:** modify `rust/crates/vfs-shim/src/fuse_client.rs`; create
`rust/crates/vfs-shim/src/regclient.rs`.

**Interfaces:**
```rust
pub fn enabled() -> bool;   // VFS_REGISTRY=1 and fuse_client::global() is Some (OnceLock)
pub fn lookup(path: &str) -> Result<(Lookup, bool), i32>;   // cached by version
pub fn key(path: &str) -> Result<Option<Node>, i32>;         // cached by version
pub fn set_value(..) / delete_value(..) / create_key(..) / delete_key(..) / rename_key(..) -> Result<(), i32>;
pub fn changed(path: &str, subtree: bool, since: u64) -> Result<(bool, u64), i32>;
```
- **Cache:** per path, tagged with the version. Every reply carries the current version, and any
  newer version invalidates the whole cache. This is simpler, and correct across processes.
- **Errors:** a ring error on a read returns `Err`, and callers then fall back to pass-through
  (Review Focus 4). On a write, the caller returns `STATUS_UNSUCCESSFUL`.

- [ ] Tests: Windows-only, in `vfs-shim/tests/regclient.rs`, using `tests/fakedirector`
  extended with the registry opcodes over a real `vfs_director::RegistryHost`. They cover:
  - a round trip of every call;
  - cache invalidation across two clients;
  - a dead director: reads give `Err`, writes give `Err`.
- [ ] Build with `cargo xwin test --no-run -p vfs-shim --target x86_64-pc-windows-msvc`, run the
  test exe under GE-Proton wine in a throwaway prefix inside the build dir, and commit.

### Task 9: shim — key handles, path resolution, open and create

**Files:**
- Create `rust/crates/vfs-shim/src/regkeys.rs`.
- Modify `hook.rs` (hooks for `NtOpenKey`, `NtOpenKeyEx`, `NtCreateKey`, `NtDuplicateObject`;
  extend `NtClose` and `NtQueryObject`).
- Modify `ntdef.rs` (function types).
- Modify `hookstats.rs` (new `Hook` variants).

**Model:**
- **Synthetic key handles** use tag 2^46 (`REG_TAG`). The table maps handle →
  `{ canonical path, access, real: Option<HANDLE> /* private real handle opened via the trampoline */ }`.
- **Pass-through handles** go in a `HashMap<HANDLE, KeyRec>` holding path and access.
- **Path of a root handle:** from either table. For an untracked real handle, ask
  `NtQueryKey(KeyNameInformation)` through the trampoline, then `vfs_registry::canonical`.
- **The user SID** is read once from the process token (`NtOpenProcessToken`,
  `NtQueryInformationToken(TokenUser)`) and converted to its string form.

**Open and create:**
- Resolve the path. If it is not virtualised, or the registry is disabled, call the trampoline.
- Look the path up in the overlay:
  - nothing at or below it: call the trampoline, record the result as pass-through, and return
    the real handle;
  - otherwise: return a synthetic handle. Open the real key privately (read-only) if it exists;
    absent is fine for overlay-created keys. A tombstone gives `STATUS_OBJECT_NAME_NOT_FOUND`
    for open.
- **Create:** if the real key and overlay key are both absent, or the path is tombstoned,
  `REG_CREATE_KEY` and return a synthetic handle with `Disposition = REG_CREATED_NEW_KEY`.
  Otherwise open as above with `REG_OPENED_EXISTING_KEY`.
- **Write access refused by the real key** (spec §3.2 "Desired access"): when the real open fails
  with `STATUS_ACCESS_DENIED` and the requested access has write rights, open the real key
  read-only and return a synthetic handle with the requested access.

**Other hooks:**
- `NtDuplicateObject`: a duplicate of a tracked handle is tracked. A synthetic source gives a new
  synthetic handle in the same process (cross-process duplication of synthetic handles is refused
  with `STATUS_NOT_SUPPORTED`).
- `NtClose`: drop the record and close the private real handle.
- `NtQueryObject(ObjectNameInformation)` on a synthetic key returns `to_nt(path)`.

- [ ] Tests: Windows-only, under Wine, using the fake director. They cover:
  - an untouched key gives a real handle (verified with `NtQueryObject` on the type);
  - a key with overlay content gives a synthetic handle;
  - an overlay-only key opens; a tombstoned key reports not found;
  - create dispositions;
  - an access-denied fallback, simulated against a real key with a restrictive DACL;
  - duplicate and close;
  - the object name.
- [ ] Implement, build, run, commit.

### Task 10: shim — query hooks

**Files:** modify `hook.rs` and `regkeys.rs` (hooks for `NtQueryKey`, `NtEnumerateKey`,
`NtQueryValueKey`, `NtEnumerateValueKey`, `NtQueryMultipleValueKey`).

**Behaviour:**
- On a pass-through handle whose path is still untouched: call the trampoline.
- Otherwise, build the `RealKey` from the private (or pass-through) real handle through
  trampolines: enumerate its subkeys and values with the Basic and Full classes, and its
  class/last-write via `KeyFullInformation`.
- Then `vfs_registry::merge`, then write with the `layout` functions.
- **Caching:** cache the `RealKey` per handle for the duration of one enumeration sequence (index
  0 starts a fresh snapshot), so index-based enumeration is O(n).

- [ ] Tests (Wine): every class against the expected layout; partial buffers; enumeration
  across real and overlay entries; tombstoned names hidden.
- [ ] Implement, build, run, commit.

### Task 11: shim — write hooks and copy-on-write

**Files:** modify `hook.rs` and `regkeys.rs` (hooks for `NtSetValueKey`, `NtDeleteValueKey`,
`NtDeleteKey`, `NtRenameKey`, `NtSetInformationKey`, `NtFlushKey`).

**Behaviour:**
- **Access check:** the handle's access must include `KEY_SET_VALUE`, `DELETE` or
  `KEY_CREATE_SUB_KEY` as appropriate; otherwise `STATUS_ACCESS_DENIED`.
- **The write itself:** goes to the director (for example `REG_SET_VALUE`). The real call is
  never made for a virtualised path. That applies to pass-through handles too: copy-on-write. The
  path then counts as touched, so later queries on the same handle merge (Review Focus 3).
- **`NtDeleteKey`:** `REG_DELETE_KEY`. The handle stays valid but marked deleted; later
  operations return `STATUS_KEY_DELETED`, as Windows does.
- **`NtRenameKey`:** leaf rename via `REG_RENAME_KEY`.
- **`NtSetInformationKey`:** last-write time and other information classes are accepted and kept
  in the overlay node, or ignored where they have no overlay meaning.
- **`NtFlushKey`:** `STATUS_SUCCESS` for virtual keys.

- [ ] Tests (Wine):
  - set then query through the same real handle (Review Focus 3);
  - the real key unchanged afterwards (read back with the hooks disabled, through the
    trampoline);
  - delete then a later operation gives `STATUS_KEY_DELETED`;
  - writes with insufficient access are denied;
  - a dead director makes writes fail (`STATUS_UNSUCCESSFUL`) with the real key unchanged.
- [ ] Implement, build, run, commit.

### Task 12: shim — notifications, unsupported calls, stats

**Files:** modify `hook.rs` and `regkeys.rs`; create `regnotify.rs`.

**Notifications:**
- `NtNotifyChangeKey` and `NtNotifyChangeMultipleKeys` on synthetic handles register a waiter:
  event, APC, or IO status block, with an asynchronous `STATUS_PENDING` result as the real call
  gives.
- One notifier thread, alive only while waiters exist, polls `REG_CHANGED` every 250 ms (R1) and
  completes waiters: it sets the IOSB to `STATUS_NOTIFY_ENUM_DIR` or `STATUS_SUCCESS` as Windows
  does, signals the event, and queues the APC.
- On pass-through handles: call the trampoline.

**Unsupported on synthetic keys** (spec §3.6), returning `STATUS_NOT_SUPPORTED`:
`NtCreateKeyTransacted`, `NtOpenKeyTransacted(Ex)`, `NtLoadKey*`, `NtSaveKey*`, `NtReplaceKey`,
`NtRestoreKey`, `NtCompressKey`, `NtLockRegistryKey`. `NtQuerySecurityObject` returns the real or
parent descriptor; `NtSetSecurityObject` is accepted and ignored.

**Stats:** hookstats rows for every new hook, plus counts of virtual and pass-through handles and
read fallbacks.

- [ ] Tests (Wine): a notify fires on an overlay change (event and APC forms); the unsupported
  calls give `STATUS_NOT_SUPPORTED`; stats rows appear.
- [ ] Implement, build, run, commit.

### Task 13: end-to-end fixture and test

**Files:**
- Create `rust/crates/vfs-fixture-registry/` (Windows exe).
- Modify `bin/build-windows` (build it).
- Create `rust/crates/vfs-embed/tests/proton_registry.rs` (`#![cfg(unix)]`, `#[ignore]`, as
  `proton_launch.rs` does).

**The fixture** runs a scripted sequence under a scratch key
(`HKCU\Software\AetherVfsRegistryTest\<run id>`) through Win32 (`RegCreateKeyExW`,
`RegSetValueExW` with every type, `RegQueryValueExW` with small and large buffers,
`RegEnumKeyExW`, `RegEnumValueW`, `RegQueryInfoKeyW`, `RegDeleteValueW`, `RegDeleteKeyW`,
`RegRenameKey`, `RegNotifyChangeKeyValue`), and also the same under an HKLM scratch key. It
prints a canonical transcript to stdout: every call's status, returned sizes, and data bytes as
hex.

**The test:**
1. Run the fixture in a session **without** a registry layer, giving transcript A. Delete the
   scratch keys afterwards, through a cleanup mode of the fixture.
2. Run it in a session **with** a registry layer (an in-memory provider), giving transcript B.
3. Assert A == B byte for byte.
4. Run the fixture's "probe" mode without the layer: the scratch keys must not exist in the real
   registry.
5. Run "probe" with the layer: the keys exist (persisted across processes in the same session).
   Detach and re-attach the same provider (as a new session would load it): still there.

- [ ] Build with `bin/build-windows --release`, run `cargo test -p vfs-embed --test
  proton_registry -- --ignored`, commit.

### Task 14: Haskill — per-profile registry layer

**Files (Haskill repo):**
- Modify `crates/haskill-store/src/keys.rs` (`registry_layer_name(list, profile) =
  "haskill.{list}.{profile}.registry"`).
- Modify `crates/haskill-vfs/src/session.rs` (create or open the layer through
  `Store::write_layer`-like access and call `Session::set_registry_layer`).
- Bump the aether-vfs submodule.

- [ ] Tests:
  - the layer name;
  - `build_session` attaches it (a fake or temporary store);
  - the registry layer is not listed among the root write layers.
- [ ] **In-game check (controller):** a JoJ launch. The shim stats show registry hooks with
  virtual handles. Compare `system.reg`/`user.reg` of the prefix byte for byte before and after
  the run (excluding Wine's own timestamp header lines): no changes from the game's writes. The
  registry layer holds `overlay.reg`.
- [ ] Commit.
