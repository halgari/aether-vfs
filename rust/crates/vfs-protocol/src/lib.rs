//! Pure wire and provider contracts for the VFS stack: wire codecs, status/opcodes, and
//! the provider contract (re-exported from `vfs-provider`). No OS I/O. The vocabulary
//! is FUSE-style RPC, but nothing here touches `/dev/fuse`. Registry ops use
//! `vfs-registry`'s portable node model.
#![forbid(unsafe_code)]

pub mod shimcfg;
mod wire;
use wire::{put_str, Rd};

pub use vfs_provider::{
    bad_fh, bad_request, exists, is_dir, map_io_err, not_a_dir, not_found, not_supported,
    read_only, Access, Capabilities, CaseMatch, DirEntry, Handle, Provider, RootId, SetAttr, Stat,
    VPath, KIND_DIR, KIND_FILE, KIND_TOMBSTONE,
};
pub use vfs_provider::{
    OPEN_APPEND, OPEN_CREATE, OPEN_EXCL, OPEN_READ, OPEN_TRUNC, OPEN_WRITE, ST_BAD_FH,
    ST_BAD_REQUEST, ST_EXISTS, ST_IO_ERROR, ST_IS_DIR, ST_NOT_A_DIRECTORY, ST_NOT_FOUND,
    ST_NOT_SUPPORTED, ST_NO_SPACE, ST_OK, ST_READ_ONLY, ST_REPLY_TOO_LARGE,
};

// The opcode catalog. This is the only definition: `vfs_ipc::layout`
// re-exports it. Numbers 4 and 12 are reserved (once `materialize` and
// `register-process`); nothing sends them and the director answers
// `ST_BAD_REQUEST`. Never renumber.

pub const OP_GETATTR: u32 = 1;
pub const OP_READDIR: u32 = 2;
pub const OP_OPEN: u32 = 3;
pub const OP_READ: u32 = 5;
pub const OP_WRITE: u32 = 6;
pub const OP_SETATTR: u32 = 7;
pub const OP_RENAME: u32 = 8;
pub const OP_DELETE: u32 = 9;
pub const OP_MKDIR: u32 = 10;
pub const OP_CLOSE: u32 = 11;
pub const OP_HEARTBEAT: u32 = 13;
/// The stored spelling of a path's components: see [`encode_names_req`].
pub const OP_STORED_NAMES: u32 = 14;
/// Registry overlay ops (15-22). Requests are built by the `encode_reg_*`
/// functions below; every reply starts with the overlay version (`u64 LE`).
pub const OP_REG_LOOKUP: u32 = 15;
pub const OP_REG_KEY: u32 = 16;
pub const OP_REG_SET_VALUE: u32 = 17;
pub const OP_REG_DELETE_VALUE: u32 = 18;
pub const OP_REG_CREATE_KEY: u32 = 19;
pub const OP_REG_DELETE_KEY: u32 = 20;
pub const OP_REG_RENAME_KEY: u32 = 21;
pub const OP_REG_CHANGED: u32 = 22;

/// Every live opcode with its name, in number order.
pub const OPCODES: &[(&str, u32)] = &[
    ("getattr", OP_GETATTR),
    ("readdir", OP_READDIR),
    ("open", OP_OPEN),
    ("read", OP_READ),
    ("write", OP_WRITE),
    ("setattr", OP_SETATTR),
    ("rename", OP_RENAME),
    ("delete", OP_DELETE),
    ("mkdir", OP_MKDIR),
    ("close", OP_CLOSE),
    ("heartbeat", OP_HEARTBEAT),
    ("stored-names", OP_STORED_NAMES),
    ("reg-lookup", OP_REG_LOOKUP),
    ("reg-key", OP_REG_KEY),
    ("reg-set-value", OP_REG_SET_VALUE),
    ("reg-delete-value", OP_REG_DELETE_VALUE),
    ("reg-create-key", OP_REG_CREATE_KEY),
    ("reg-delete-key", OP_REG_DELETE_KEY),
    ("reg-rename-key", OP_REG_RENAME_KEY),
    ("reg-changed", OP_REG_CHANGED),
];

/// Every status code with its name, in number order.
pub const STATUSES: &[(&str, i32)] = &[
    ("ok", ST_OK),
    ("not-found", ST_NOT_FOUND),
    ("not-a-directory", ST_NOT_A_DIRECTORY),
    ("bad-request", ST_BAD_REQUEST),
    ("io-error", ST_IO_ERROR),
    ("is-dir", ST_IS_DIR),
    ("bad-fh", ST_BAD_FH),
    ("no-space", ST_NO_SPACE),
    ("not-supported", ST_NOT_SUPPORTED),
    ("read-only", ST_READ_ONLY),
    ("exists", ST_EXISTS),
    ("reply-too-large", ST_REPLY_TOO_LARGE),
];

/// Ring/request flag: prefer bulk-arena READ (data in shared arena, not ring payload).
pub const FLAG_READ_BULK: u32 = 0x1;
/// High bit on READ response `bytes_read` means bulk: data lives at arena_offset.
pub const READ_RESP_BULK_BIT: u32 = 0x8000_0000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrResp {
    pub found: bool,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryWire {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenResp {
    pub fh: u64,
    pub size: u64,
    pub is_dir: bool,
    /// The bytes behind `fh` cannot change while it is open (the provider
    /// that serves it says so: `Provider::is_immutable`). A client may cache
    /// what it reads through such a handle. Always `false` for a write open.
    /// Carried in what was padding, so a director that predates it says
    /// `false` and a client that predates it ignores it.
    pub immutable: bool,
    /// The director's mount generation when it opened `fh`: bumped whenever
    /// a root's provider is replaced. Two immutable opens of one path with
    /// the same generation (and size) are the same content; across a remount
    /// they need not be. `0` from a director that predates it.
    pub mount_gen: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadReq {
    pub fh: u64,
    pub offset: u64,
    pub len: u32,
}

/// Path-carrying request: `root:u32 | path_utf8…`. Used by GETATTR, READDIR
/// and DELETE.
///
/// **Stage 2b task 5 widened this from a bare UTF-8 path.** A session
/// virtualizes several roots, and a path alone cannot say which one it belongs
/// to — the shim classifies an open as "root 1" and, before this, had no field
/// in which to say so, leaving `dispatch_director` to assume `RootId::DEFAULT`
/// for every request. The root rides ahead of the path rather than in a
/// parallel opcode so there is one code path per operation, not two that can
/// drift.
///
/// The layout is a contract with the injected DLL: a shim built against the
/// old shape would have its first four path bytes read as a root id, which is
/// plausible-looking garbage rather than a clean failure. `vfs_ipc::layout`'s
/// `VERSION` was bumped to 2 in the same change so such a shim is rejected at
/// ring open instead of misparsing every request.
pub fn encode_path_req(root: u32, vpath: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(4 + vpath.len());
    b.extend_from_slice(&root.to_le_bytes());
    b.extend_from_slice(vpath.as_bytes());
    b
}

pub fn decode_path_req(payload: &[u8]) -> Option<(u32, String)> {
    let mut r = Rd(payload);
    let root = r.u32()?;
    let path = r.rest_str()?.to_string();
    Some((root, path))
}

pub fn encode_getattr_resp(r: &AttrResp) -> Vec<u8> {
    let mut b = Vec::with_capacity(18);
    b.push(r.found as u8);
    b.push(r.is_dir as u8);
    b.extend_from_slice(&r.size.to_le_bytes());
    b.extend_from_slice(&r.mtime.to_le_bytes());
    b
}

pub fn decode_getattr_resp(p: &[u8]) -> Option<AttrResp> {
    let mut r = Rd(p);
    let found = r.flag()?;
    let is_dir = r.flag()?;
    let size = r.u64()?;
    let mtime = r.u64()? as i64;
    Some(AttrResp {
        found,
        is_dir,
        size,
        mtime,
    })
}

pub fn encode_readdir_resp(entries: &[DirEntryWire]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        let name = e.name.as_bytes();
        b.extend_from_slice(&(name.len() as u32).to_le_bytes());
        b.extend_from_slice(name);
        b.push(e.is_dir as u8);
        b.extend_from_slice(&e.size.to_le_bytes());
        b.extend_from_slice(&e.mtime.to_le_bytes());
    }
    b
}

pub fn decode_readdir_resp(p: &[u8]) -> Option<Vec<DirEntryWire>> {
    let mut r = Rd(p);
    let count = r.u32()?;
    let mut out = Vec::new();
    for _ in 0..count {
        let name = r.str()?.to_string();
        let is_dir = r.flag()?;
        let size = r.u64()?;
        let mtime = r.u64()? as i64;
        out.push(DirEntryWire {
            name,
            is_dir,
            size,
            mtime,
        });
    }
    Some(out)
}

/// OPEN req: `root:u32 | flags:u32 | path_utf8…`
///
/// See [`encode_path_req`] for why the root leads every path-carrying payload.
pub fn encode_open_req(root: u32, flags: u32, path: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + path.len());
    b.extend_from_slice(&root.to_le_bytes());
    b.extend_from_slice(&flags.to_le_bytes());
    b.extend_from_slice(path.as_bytes());
    b
}

/// Returns `(root, flags, path)`.
pub fn decode_open_req(p: &[u8]) -> Option<(u32, u32, String)> {
    let mut r = Rd(p);
    let root = r.u32()?;
    let flags = r.u32()?;
    let path = r.rest_str()?.to_string();
    Some((root, flags, path))
}

/// Bit 0 of an OPEN reply's flags byte: [`OpenResp::immutable`].
pub const OPEN_RESP_IMMUTABLE: u8 = 0x01;

/// OPEN resp: `fh:u64 | size:u64 | is_dir:u8 | flags:u8 | pad[2] | mount_gen:u32`
///
/// `flags` and `mount_gen` were padding (zero) until the shim's read cache
/// needed them, so the layout and length are unchanged and either side may
/// be older than the other: an old director's reply decodes as mutable,
/// generation 0, which a cache never serves from.
pub fn encode_open_resp(r: &OpenResp) -> Vec<u8> {
    let mut b = Vec::with_capacity(24);
    b.extend_from_slice(&r.fh.to_le_bytes());
    b.extend_from_slice(&r.size.to_le_bytes());
    b.push(r.is_dir as u8);
    b.push(if r.immutable { OPEN_RESP_IMMUTABLE } else { 0 });
    b.extend_from_slice(&[0u8; 2]);
    b.extend_from_slice(&r.mount_gen.to_le_bytes());
    b
}

pub fn decode_open_resp(p: &[u8]) -> Option<OpenResp> {
    let mut r = Rd(p);
    let fh = r.u64()?;
    let size = r.u64()?;
    let is_dir = r.flag()?;
    let immutable = r.u8()? & OPEN_RESP_IMMUTABLE != 0;
    r.take(2)?; // padding
    let mount_gen = r.u32()?;
    Some(OpenResp {
        fh,
        size,
        is_dir,
        immutable,
        mount_gen,
    })
}

/// READ req: `fh:u64 | offset:u64 | len:u32 | pad:u32`
pub fn encode_read_req(r: &ReadReq) -> Vec<u8> {
    let mut b = Vec::with_capacity(24);
    b.extend_from_slice(&r.fh.to_le_bytes());
    b.extend_from_slice(&r.offset.to_le_bytes());
    b.extend_from_slice(&r.len.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b
}

pub fn decode_read_req(p: &[u8]) -> Option<ReadReq> {
    // The trailing `pad:u32` is not required: a 20-byte request decodes.
    let mut r = Rd(p);
    let fh = r.u64()?;
    let offset = r.u64()?;
    let len = r.u32()?;
    Some(ReadReq { fh, offset, len })
}

/// READ resp: `bytes_read:u32 | pad:u32 | data[bytes_read]`
pub fn encode_read_resp(data: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + data.len());
    b.extend_from_slice(&(data.len() as u32).to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(data);
    b
}

pub fn decode_read_resp(p: &[u8]) -> Option<Vec<u8>> {
    let mut r = Rd(p);
    let n = r.u32()? as usize;
    r.take(4)?; // pad
    Some(r.take(n)?.to_vec())
}

/// **A3:** copy READ response data into `out` without allocating a second Vec.
/// Returns bytes copied (may be less than `out.len()` on short/EOF reads).
/// Inline responses only (not bulk).
pub fn decode_read_resp_into(p: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut r = Rd(p);
    let raw = r.u32()?;
    if raw & READ_RESP_BULK_BIT != 0 {
        return None; // use decode_read_bulk_resp + arena
    }
    r.take(4)?; // pad
    let data = r.take(raw as usize)?;
    let n = data.len().min(out.len());
    out[..n].copy_from_slice(&data[..n]);
    Some(n)
}

/// Bulk READ response: `bytes_read|BULK_BIT : u32 | pad:u32 | arena_offset:u64`
pub fn encode_read_resp_bulk(bytes_read: u32, arena_offset: u64) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.extend_from_slice(&(bytes_read | READ_RESP_BULK_BIT).to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&arena_offset.to_le_bytes());
    b
}

/// Returns `(bytes_read, arena_offset)` for a bulk response, or `None` if inline/malformed.
pub fn decode_read_bulk_resp(p: &[u8]) -> Option<(u32, u64)> {
    let mut r = Rd(p);
    let raw = r.u32()?;
    if raw & READ_RESP_BULK_BIT == 0 {
        return None;
    }
    r.take(4)?; // pad
    let off = r.u64()?;
    Some((raw & !READ_RESP_BULK_BIT, off))
}

pub fn is_read_resp_bulk(p: &[u8]) -> bool {
    Rd(p).u32().is_some_and(|raw| raw & READ_RESP_BULK_BIT != 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteReq {
    pub fh: u64,
    pub offset: u64,
    pub len: u32,
}

/// WRITE req: `fh:u64 | offset:u64 | len:u32 | pad:u32 | data[len]`
pub fn encode_write_req(r: &WriteReq, data: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(24 + data.len());
    b.extend_from_slice(&r.fh.to_le_bytes());
    b.extend_from_slice(&r.offset.to_le_bytes());
    b.extend_from_slice(&(data.len() as u32).to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(data);
    b
}

pub fn decode_write_req(p: &[u8]) -> Option<(WriteReq, Vec<u8>)> {
    let mut r = Rd(p);
    let fh = r.u64()?;
    let offset = r.u64()?;
    let len = r.u32()?;
    r.take(4)?; // pad
    let data = r.take(len as usize)?.to_vec();
    Some((WriteReq { fh, offset, len }, data))
}

/// WRITE resp: `bytes_written:u32 | pad:u32`
pub fn encode_write_resp(n: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(8);
    b.extend_from_slice(&n.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b
}

pub fn decode_write_resp(p: &[u8]) -> Option<u32> {
    Rd(p).u32()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetattrReq {
    pub fh: u64,
    pub size: u64,
}

/// MKDIR req: `root:u32 | mode:u32 | path_utf8…`
///
/// See [`encode_path_req`] for why the root leads every path-carrying payload.
pub fn encode_mkdir_req(root: u32, mode: u32, path: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + path.len());
    b.extend_from_slice(&root.to_le_bytes());
    b.extend_from_slice(&mode.to_le_bytes());
    b.extend_from_slice(path.as_bytes());
    b
}

/// Returns `(root, mode, path)`.
pub fn decode_mkdir_req(p: &[u8]) -> Option<(u32, u32, String)> {
    let mut r = Rd(p);
    let root = r.u32()?;
    let mode = r.u32()?;
    let path = r.rest_str()?.to_string();
    Some((root, mode, path))
}

/// STORED_NAMES req: `root:u32 | skip:u32 | path_utf8`. Asks how the
/// components of `path` from the `skip`-th on are spelled where they are
/// stored. The reply is those names joined by `/`, one per component asked
/// about (a name has no `/` in it); a component nothing has comes back as it
/// was sent.
pub fn encode_names_req(root: u32, skip: u32, path: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + path.len());
    b.extend_from_slice(&root.to_le_bytes());
    b.extend_from_slice(&skip.to_le_bytes());
    b.extend_from_slice(path.as_bytes());
    b
}

/// Returns `(root, skip, path)`.
pub fn decode_names_req(p: &[u8]) -> Option<(u32, u32, String)> {
    let mut r = Rd(p);
    let root = r.u32()?;
    let skip = r.u32()?;
    let path = r.rest_str()?.to_string();
    Some((root, skip, path))
}

/// STORED_NAMES reply: the names joined by `/` (a name has no `/` in it).
pub fn encode_names_resp(names: &[String]) -> Vec<u8> {
    names.join("/").into_bytes()
}

/// The names of a STORED_NAMES reply, or `None` if it is not UTF-8. An empty
/// payload is one empty name, as `str::split` has it.
pub fn decode_names_resp(p: &[u8]) -> Option<Vec<String>> {
    let text = core::str::from_utf8(p).ok()?;
    Some(text.split('/').map(str::to_string).collect())
}

/// RENAME req: `root:u32 | from_len:u32 | from_utf8 | to_utf8`
///
/// One root, not two: `Director::rename` resolves both sides against a single
/// root, and a cross-root rename has no meaning in the provider contract.
pub fn encode_rename_req(root: u32, from: &str, to: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + from.len() + to.len());
    b.extend_from_slice(&root.to_le_bytes());
    b.extend_from_slice(&(from.len() as u32).to_le_bytes());
    b.extend_from_slice(from.as_bytes());
    b.extend_from_slice(to.as_bytes());
    b
}

/// Returns `(root, from, to)`.
pub fn decode_rename_req(p: &[u8]) -> Option<(u32, String, String)> {
    let mut r = Rd(p);
    let root = r.u32()?;
    let from_len = r.u32()? as usize;
    let from = core::str::from_utf8(r.take(from_len)?).ok()?.to_string();
    let to = r.rest_str()?.to_string();
    Some((root, from, to))
}

/// SETATTR req: `fh:u64 | size:u64`
pub fn encode_setattr_req(r: &SetattrReq) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.extend_from_slice(&r.fh.to_le_bytes());
    b.extend_from_slice(&r.size.to_le_bytes());
    b
}

pub fn decode_setattr_req(p: &[u8]) -> Option<SetattrReq> {
    let mut r = Rd(p);
    let fh = r.u64()?;
    let size = r.u64()?;
    Some(SetattrReq { fh, size })
}

pub fn encode_close_req(fh: u64) -> Vec<u8> {
    fh.to_le_bytes().to_vec()
}

pub fn decode_close_req(p: &[u8]) -> Option<u64> {
    Rd(p).u64()
}

// ---------------------------------------------------------------------------
// Registry overlay codecs. Strings are `len:u32 LE | utf8`; booleans are one
// byte, 0 or 1. Every decoder consumes its input exactly and returns `None`
// for anything else (truncation, trailing bytes, bad UTF-8, out-of-range
// flags or lengths); none of them can panic or over-allocate.
// ---------------------------------------------------------------------------

use vfs_registry::overlay::{MAX_DATA, MAX_KEY_NAME, MAX_VALUE_NAME};
use vfs_registry::{Child, Node, Value};

/// A request that is only a path (also `REG_KEY`, `REG_LOOKUP`, `REG_DELETE_KEY`).
pub fn encode_reg_path(path: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(4 + path.len());
    put_str(&mut b, path);
    b
}

pub fn decode_reg_path(b: &[u8]) -> Option<&str> {
    let mut r = Rd(b);
    let p = r.str()?;
    r.done()?;
    Some(p)
}

/// `REG_SET_VALUE` req: `path | name | ty:u32 | data_len:u32 | data`.
pub fn encode_reg_set_value(path: &str, name: &str, ty: u32, data: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(16 + path.len() + name.len() + data.len());
    put_str(&mut b, path);
    put_str(&mut b, name);
    b.extend_from_slice(&ty.to_le_bytes());
    b.extend_from_slice(&(data.len() as u32).to_le_bytes());
    b.extend_from_slice(data);
    b
}

/// Returns `(path, name, ty, data)`.
pub fn decode_reg_set_value(b: &[u8]) -> Option<(&str, &str, u32, &[u8])> {
    let mut r = Rd(b);
    let path = r.str()?;
    let name = r.str_max(MAX_VALUE_NAME)?;
    let ty = r.u32()?;
    let data = r.bytes(MAX_DATA)?;
    r.done()?;
    Some((path, name, ty, data))
}

/// `REG_DELETE_VALUE` req: `path | name`.
pub fn encode_reg_delete_value(path: &str, name: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + path.len() + name.len());
    put_str(&mut b, path);
    put_str(&mut b, name);
    b
}

pub fn decode_reg_delete_value(b: &[u8]) -> Option<(&str, &str)> {
    let mut r = Rd(b);
    let path = r.str()?;
    let name = r.str_max(MAX_VALUE_NAME)?;
    r.done()?;
    Some((path, name))
}

/// `REG_CREATE_KEY` req: `path | volatile:u8`.
pub fn encode_reg_create_key(path: &str, volatile: bool) -> Vec<u8> {
    let mut b = encode_reg_path(path);
    b.push(volatile as u8);
    b
}

pub fn decode_reg_create_key(b: &[u8]) -> Option<(&str, bool)> {
    let mut r = Rd(b);
    let path = r.str()?;
    let v = r.bool()?;
    r.done()?;
    Some((path, v))
}

/// `REG_RENAME_KEY` req: `path | new_leaf`.
pub fn encode_reg_rename_key(path: &str, new_leaf: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + path.len() + new_leaf.len());
    put_str(&mut b, path);
    put_str(&mut b, new_leaf);
    b
}

pub fn decode_reg_rename_key(b: &[u8]) -> Option<(&str, &str)> {
    let mut r = Rd(b);
    let path = r.str()?;
    let leaf = r.str_max(MAX_KEY_NAME)?;
    r.done()?;
    Some((path, leaf))
}

/// `REG_CHANGED` req: `path | subtree:u8 | since:u64`.
pub fn encode_reg_changed(path: &str, subtree: bool, version: u64) -> Vec<u8> {
    let mut b = encode_reg_path(path);
    b.push(subtree as u8);
    b.extend_from_slice(&version.to_le_bytes());
    b
}

/// Returns `(path, subtree, since)`.
pub fn decode_reg_changed(b: &[u8]) -> Option<(&str, bool, u64)> {
    let mut r = Rd(b);
    let path = r.str()?;
    let subtree = r.bool()?;
    let since = r.u64()?;
    r.done()?;
    Some((path, subtree, since))
}

/// `REG_CHANGED` reply: `version:u64 | changed:u8`.
pub fn encode_reg_changed_reply(changed: bool, version: u64) -> Vec<u8> {
    let mut b = version.to_le_bytes().to_vec();
    b.push(changed as u8);
    b
}

/// Returns `(changed, version)`.
pub fn decode_reg_changed_reply(b: &[u8]) -> Option<(bool, u64)> {
    let mut r = Rd(b);
    let version = r.u64()?;
    let changed = r.bool()?;
    r.done()?;
    Some((changed, version))
}

/// Reply of the mutating ops: just the overlay version after the change.
pub fn encode_reg_version_reply(version: u64) -> Vec<u8> {
    version.to_le_bytes().to_vec()
}

pub fn decode_reg_version_reply(b: &[u8]) -> Option<u64> {
    let mut r = Rd(b);
    let v = r.u64()?;
    r.done()?;
    Some(v)
}

/// `REG_LOOKUP` reply: `version:u64 | state:u8 | below:u8`. State is 0 absent,
/// 1 present, 2 present-created, 3 tombstoned; `below` says the overlay has
/// anything underneath the key.
pub fn encode_reg_lookup_reply(state: u8, below: bool, version: u64) -> Vec<u8> {
    let mut b = version.to_le_bytes().to_vec();
    b.push(state);
    b.push(below as u8);
    b
}

/// Returns `(state, below, version)`.
pub fn decode_reg_lookup_reply(b: &[u8]) -> Option<(u8, bool, u64)> {
    let mut r = Rd(b);
    let version = r.u64()?;
    let state = r.u8()?;
    if state > 3 {
        return None;
    }
    let below = r.bool()?;
    r.done()?;
    Some((state, below, version))
}

/// `REG_KEY` reply: `version:u64 | has_node:u8 | node?`, where a node is
/// `created:u8 | volatile:u8 | last_write:u64 | nvalues:u32 | values |
/// ntombstones:u32 | names | nchildren:u32 | children`; a value is
/// `name | ty:u32 | data_len:u32 | data`, a child `folded | spelling | state:u8`
/// (0 present, 1 tombstone).
pub fn encode_reg_key_reply(node: Option<&Node>, version: u64) -> Vec<u8> {
    let mut b = version.to_le_bytes().to_vec();
    let Some(n) = node else {
        b.push(0);
        return b;
    };
    b.push(1);
    b.push(n.created as u8);
    b.push(n.volatile as u8);
    b.extend_from_slice(&n.last_write.to_le_bytes());
    b.extend_from_slice(&(n.values.len() as u32).to_le_bytes());
    for v in &n.values {
        put_str(&mut b, &v.name);
        b.extend_from_slice(&v.ty.to_le_bytes());
        b.extend_from_slice(&(v.data.len() as u32).to_le_bytes());
        b.extend_from_slice(&v.data);
    }
    b.extend_from_slice(&(n.value_tombstones.len() as u32).to_le_bytes());
    for t in &n.value_tombstones {
        put_str(&mut b, t);
    }
    b.extend_from_slice(&(n.children.len() as u32).to_le_bytes());
    for (folded, (spelling, state)) in &n.children {
        put_str(&mut b, folded);
        put_str(&mut b, spelling);
        b.push(match state {
            Child::Present => 0,
            Child::Tombstone => 1,
        });
    }
    b
}

/// Returns `(node, version)`; the node is `None` for a "no node" reply.
pub fn decode_reg_key_reply(b: &[u8]) -> Option<(Option<Node>, u64)> {
    let mut r = Rd(b);
    let version = r.u64()?;
    if !r.bool()? {
        r.done()?;
        return Some((None, version));
    }
    let mut n = Node {
        created: r.bool()?,
        volatile: r.bool()?,
        last_write: r.u64()?,
        ..Node::default()
    };
    // Counts are not trusted for allocation: every element takes at least
    // 4 bytes, so a count beyond the remaining input fails on the way.
    let count = r.u32()?;
    for _ in 0..count {
        let name = r.str_max(MAX_VALUE_NAME)?.to_string();
        let ty = r.u32()?;
        let data = r.bytes(MAX_DATA)?.to_vec();
        n.values.push(Value { name, ty, data });
    }
    let count = r.u32()?;
    for _ in 0..count {
        n.value_tombstones
            .push(r.str_max(MAX_VALUE_NAME)?.to_string());
    }
    let count = r.u32()?;
    for _ in 0..count {
        let folded = r.str_max(MAX_KEY_NAME)?.to_string();
        let spelling = r.str_max(MAX_KEY_NAME)?.to_string();
        let state = match r.u8()? {
            0 => Child::Present,
            1 => Child::Tombstone,
            _ => return None,
        };
        n.children.insert(folded, (spelling, state));
    }
    r.done()?;
    Some((Some(n), version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_resp_roundtrip() {
        let names = vec!["Data".to_string(), "Skyrim.esm".to_string()];
        assert_eq!(encode_names_resp(&names), b"Data/Skyrim.esm");
        assert_eq!(decode_names_resp(&encode_names_resp(&names)), Some(names));
        assert_eq!(decode_names_resp(&[0xff]), None);
    }

    #[test]
    fn open_req_roundtrip() {
        let p = encode_open_req(0, OPEN_READ, "Data/Skyrim.esm");
        let (root, f, path) = decode_open_req(&p).unwrap();
        assert_eq!(root, 0);
        assert_eq!(f, OPEN_READ);
        assert_eq!(path, "Data/Skyrim.esm");
    }

    /// Stage 2b task 5, step 1: the same relative path carried for two
    /// different roots must produce two distinguishable payloads, and each
    /// must decode back to its own root. Without this the shim can classify a
    /// path as belonging to root 1 and has no field in which to say so.
    #[test]
    fn path_carrying_payloads_carry_the_root() {
        let a = encode_path_req(0, "same.txt");
        let b = encode_path_req(1, "same.txt");
        assert_ne!(a, b, "two roots must not encode to identical bytes");
        assert_eq!(decode_path_req(&a), Some((0, "same.txt".to_string())));
        assert_eq!(decode_path_req(&b), Some((1, "same.txt".to_string())));

        let o = encode_open_req(7, OPEN_READ, "same.txt");
        assert_eq!(
            decode_open_req(&o),
            Some((7, OPEN_READ, "same.txt".to_string()))
        );

        let m = encode_mkdir_req(2, 493, "sub/dir");
        assert_eq!(decode_mkdir_req(&m), Some((2, 493, "sub/dir".to_string())));

        let r = encode_rename_req(3, "old.txt", "new.txt");
        assert_eq!(
            decode_rename_req(&r),
            Some((3, "old.txt".to_string(), "new.txt".to_string()))
        );
    }

    /// The exact byte layout, pinned. This is a contract with the injected
    /// DLL: a shim built against the pre-task-5 shape (bare path / bare
    /// `flags|path`) would have its first four path bytes decoded as a root
    /// id. `vfs_ipc::layout::VERSION` was bumped to 2 in the same change so
    /// that shim is rejected at ring open rather than silently misparsed —
    /// see `vfs_ipc::ring::tests::open_rejects_a_stale_wire_version`.
    #[test]
    fn root_leads_the_wire_layout() {
        assert_eq!(encode_path_req(1, "a"), vec![1, 0, 0, 0, b'a']);
        assert_eq!(
            encode_open_req(1, 2, "a"),
            vec![1, 0, 0, 0, 2, 0, 0, 0, b'a']
        );
        assert_eq!(
            encode_mkdir_req(1, 2, "a"),
            vec![1, 0, 0, 0, 2, 0, 0, 0, b'a']
        );
        assert_eq!(
            encode_rename_req(1, "a", "b"),
            vec![1, 0, 0, 0, 1, 0, 0, 0, b'a', b'b']
        );
    }

    #[test]
    fn open_resp_roundtrip() {
        let r = OpenResp {
            fh: 42,
            size: 1000,
            is_dir: false,
            immutable: true,
            mount_gen: 0xA1B2_C3D4,
        };
        assert_eq!(decode_open_resp(&encode_open_resp(&r)), Some(r));
    }

    /// A reply from a director that predates the flags byte has zeros there:
    /// it must decode as mutable, generation 0 — never as cacheable.
    #[test]
    fn an_open_reply_with_zero_padding_is_mutable() {
        let mut b = Vec::new();
        b.extend_from_slice(&7u64.to_le_bytes());
        b.extend_from_slice(&9u64.to_le_bytes());
        b.extend_from_slice(&[0u8; 8]);
        let r = decode_open_resp(&b).unwrap();
        assert_eq!((r.fh, r.size, r.immutable, r.mount_gen), (7, 9, false, 0));
    }

    #[test]
    fn read_req_resp_roundtrip() {
        let req = ReadReq {
            fh: 7,
            offset: 10,
            len: 4,
        };
        assert_eq!(decode_read_req(&encode_read_req(&req)), Some(req));
        let data = b"abcd";
        assert_eq!(
            decode_read_resp(&encode_read_resp(data)).as_deref(),
            Some(&data[..])
        );
        let mut out = [0u8; 8];
        assert_eq!(
            decode_read_resp_into(&encode_read_resp(data), &mut out),
            Some(4)
        );
        assert_eq!(&out[..4], b"abcd");
    }

    #[test]
    fn write_req_resp_roundtrip() {
        let req = WriteReq {
            fh: 7,
            offset: 10,
            len: 3,
        };
        let data = b"abc";
        let encoded = encode_write_req(&req, data);
        let (decoded_req, decoded_data) = decode_write_req(&encoded).unwrap();
        assert_eq!(decoded_req, req);
        assert_eq!(decoded_data, data);
        assert_eq!(decode_write_resp(&encode_write_resp(3)), Some(3));
    }

    #[test]
    fn close_req_roundtrip() {
        assert_eq!(decode_close_req(&encode_close_req(99)), Some(99));
    }

    #[test]
    fn mkdir_req_roundtrip() {
        let p = encode_mkdir_req(0, 493, "sub/dir");
        assert_eq!(decode_mkdir_req(&p), Some((0, 493, "sub/dir".to_string())));
    }

    #[test]
    fn rename_req_roundtrip() {
        let p = encode_rename_req(0, "old.txt", "new.txt");
        assert_eq!(
            decode_rename_req(&p),
            Some((0, "old.txt".to_string(), "new.txt".to_string()))
        );
    }

    #[test]
    fn setattr_req_roundtrip() {
        let req = SetattrReq { fh: 5, size: 100 };
        assert_eq!(decode_setattr_req(&encode_setattr_req(&req)), Some(req));
    }

    /// The wire numbers as literals, taken from the retired protocol
    /// descriptor (75db467). A change here is a wire break.
    #[test]
    fn wire_numbers_are_the_historical_ones() {
        let want: &[(&str, u32)] = &[
            ("getattr", 1),
            ("readdir", 2),
            ("open", 3),
            ("read", 5),
            ("write", 6),
            ("setattr", 7),
            ("rename", 8),
            ("delete", 9),
            ("mkdir", 10),
            ("close", 11),
            ("heartbeat", 13),
            ("stored-names", 14),
            ("reg-lookup", 15),
            ("reg-key", 16),
            ("reg-set-value", 17),
            ("reg-delete-value", 18),
            ("reg-create-key", 19),
            ("reg-delete-key", 20),
            ("reg-rename-key", 21),
            ("reg-changed", 22),
        ];
        assert_eq!(OPCODES, want);
        let sts: &[(&str, i32)] = &[
            ("ok", 0),
            ("not-found", -1),
            ("not-a-directory", -2),
            ("bad-request", -3),
            ("io-error", -4),
            ("is-dir", -5),
            ("bad-fh", -6),
            ("no-space", -7),
            ("not-supported", -8),
            ("read-only", -9),
            ("exists", -10),
            ("reply-too-large", -11),
        ];
        assert_eq!(STATUSES, sts);
        assert_eq!(OPEN_READ, 1);
        assert_eq!(OPEN_WRITE, 2);
        assert_eq!(OPEN_CREATE, 4);
        assert_eq!(OPEN_EXCL, 8);
        assert_eq!(OPEN_TRUNC, 16);
        assert_eq!(OPEN_APPEND, 32);
        assert_eq!(FLAG_READ_BULK, 1);
        assert_eq!(READ_RESP_BULK_BIT, 0x8000_0000);
        assert_eq!(OPEN_RESP_IMMUTABLE, 1);
    }

    #[test]
    fn short_buffers_decode_none() {
        assert!(decode_open_req(&[1, 2]).is_none());
        // A payload carrying only the root and no flags is still short: the
        // pre-task-5 `flags|path` shape decoded a 4-byte prefix, so a bare
        // 4-byte buffer must not now look like a valid OPEN.
        assert!(decode_open_req(&[0, 0, 0, 0]).is_none());
        assert!(decode_path_req(&[1, 2]).is_none());
        assert!(decode_mkdir_req(&[0, 0, 0, 0]).is_none());
        assert!(decode_rename_req(&[0, 0, 0, 0]).is_none());
        assert!(decode_read_req(&[0u8; 10]).is_none());
        assert!(decode_read_resp(&[1, 0, 0]).is_none());
    }

    /// The file-op decoders are deliberately lenient (unlike the registry
    /// ones): trailing bytes are ignored, any non-zero byte is true, and a
    /// READ request may omit its trailing pad. Pinned so the shared cursor
    /// cannot tighten them by accident.
    #[test]
    fn file_op_decoders_stay_lenient() {
        let mut g = encode_getattr_resp(&AttrResp {
            found: true,
            is_dir: true,
            size: 5,
            mtime: 6,
        });
        g[0] = 7;
        g[1] = 0xff;
        g.extend_from_slice(&[9, 9]);
        let a = decode_getattr_resp(&g).unwrap();
        assert!(a.found && a.is_dir);
        assert_eq!((a.size, a.mtime), (5, 6));

        let mut c = encode_close_req(3);
        c.push(0);
        assert_eq!(decode_close_req(&c), Some(3));
        let mut s = encode_setattr_req(&SetattrReq { fh: 1, size: 2 });
        s.push(0);
        assert_eq!(decode_setattr_req(&s), Some(SetattrReq { fh: 1, size: 2 }));
        let req = ReadReq {
            fh: 1,
            offset: 2,
            len: 3,
        };
        let r = encode_read_req(&req);
        assert_eq!(decode_read_req(&r[..20]), Some(req));
        assert_eq!(decode_write_resp(&encode_write_resp(4)[..4]), Some(4));
        let mut rr = encode_read_resp(b"xy");
        rr.push(0);
        assert_eq!(decode_read_resp(&rr).as_deref(), Some(&b"xy"[..]));
        let mut o = encode_open_resp(&OpenResp::default());
        o[16] = 2;
        o[18] = 0xaa;
        o.push(0);
        assert!(decode_open_resp(&o).unwrap().is_dir);
        let mut d = encode_readdir_resp(&[DirEntryWire {
            name: "a".into(),
            is_dir: true,
            size: 1,
            mtime: 2,
        }]);
        d.push(0);
        assert_eq!(decode_readdir_resp(&d).unwrap().len(), 1);
        let mut w = encode_write_req(
            &WriteReq {
                fh: 1,
                offset: 2,
                len: 2,
            },
            b"hi",
        );
        w.extend_from_slice(&[7, 7]);
        let (wr, data) = decode_write_req(&w).unwrap();
        assert_eq!((wr.len, data), (2, b"hi".to_vec()));
        let mut rq = encode_read_req(&req);
        rq.push(9);
        assert_eq!(decode_read_req(&rq), Some(req));
        // bulk read reply: trailing bytes ignored, inline decoders refuse it
        let mut bulk = encode_read_resp_bulk(5, 65536);
        bulk.push(1);
        assert_eq!(decode_read_bulk_resp(&bulk), Some((5, 65536)));
        assert!(is_read_resp_bulk(&bulk));
        assert!(decode_read_resp_into(&bulk, &mut [0u8; 8]).is_none());
        assert!(decode_read_bulk_resp(&encode_read_resp(b"ab")).is_none());
        // readdir is_dir: any non-zero byte is true
        let mut dd = encode_readdir_resp(&[DirEntryWire {
            name: "a".into(),
            is_dir: false,
            size: 1,
            mtime: 2,
        }]);
        let at = 4 + 4 + 1;
        dd[at] = 2;
        assert!(decode_readdir_resp(&dd).unwrap()[0].is_dir);
        // a READ reply whose data is cut short is still malformed
        assert!(decode_read_resp(&encode_read_resp(b"abc")[..10]).is_none());
        assert!(decode_write_req(
            &encode_write_req(
                &WriteReq {
                    fh: 1,
                    offset: 0,
                    len: 3
                },
                b"abc"
            )[..25]
        )
        .is_none());
    }

    #[test]
    fn getattr_resp_roundtrip() {
        let r = AttrResp {
            found: true,
            is_dir: false,
            size: 123,
            mtime: -7,
        };
        assert_eq!(decode_getattr_resp(&encode_getattr_resp(&r)), Some(r));
    }

    #[test]
    fn readdir_resp_roundtrip() {
        let entries = vec![
            DirEntryWire {
                name: "a.esp".into(),
                is_dir: false,
                size: 10,
                mtime: 1,
            },
            DirEntryWire {
                name: "sub".into(),
                is_dir: true,
                size: 0,
                mtime: 0,
            },
        ];
        assert_eq!(
            decode_readdir_resp(&encode_readdir_resp(&entries)),
            Some(entries)
        );
    }

    // ---- registry ops (15-22) ----

    fn sample_node() -> vfs_registry::Node {
        use vfs_registry::{Child, Node, Value};
        let mut n = Node {
            values: vec![
                Value {
                    name: "Path".into(),
                    ty: 1,
                    data: vec![1, 2, 3],
                },
                Value {
                    name: String::new(),
                    ty: 4,
                    data: vec![],
                },
            ],
            value_tombstones: vec!["gone".into()],
            created: true,
            volatile: true,
            last_write: 0x0123_4567_89ab_cdef,
            ..Node::default()
        };
        n.children
            .insert("sub".into(), ("Sub".into(), Child::Present));
        n.children
            .insert("dead".into(), ("Dead".into(), Child::Tombstone));
        n
    }

    #[test]
    fn reg_opcodes_are_15_to_22() {
        assert_eq!(
            [
                OP_REG_LOOKUP,
                OP_REG_KEY,
                OP_REG_SET_VALUE,
                OP_REG_DELETE_VALUE,
                OP_REG_CREATE_KEY,
                OP_REG_DELETE_KEY,
                OP_REG_RENAME_KEY,
                OP_REG_CHANGED
            ],
            [15, 16, 17, 18, 19, 20, 21, 22]
        );
    }

    #[test]
    fn reg_path_roundtrip_and_malformed() {
        let p = encode_reg_path("\\Registry\\Machine\\Sóftware");
        assert_eq!(decode_reg_path(&p), Some("\\Registry\\Machine\\Sóftware"));
        assert_eq!(decode_reg_path(&encode_reg_path("")), Some(""));
        assert_eq!(decode_reg_path(&[]), None);
        assert_eq!(decode_reg_path(&[1, 0, 0]), None);
        assert_eq!(
            decode_reg_path(&[5, 0, 0, 0, b'a']),
            None,
            "length past end"
        );
        let mut t = encode_reg_path("a");
        t.push(0);
        assert_eq!(decode_reg_path(&t), None, "trailing bytes");
        assert_eq!(decode_reg_path(&[1, 0, 0, 0, 0xff]), None, "bad utf8");
        assert_eq!(
            decode_reg_path(&[0xff, 0xff, 0xff, 0xff]),
            None,
            "huge length"
        );
    }

    #[test]
    fn reg_set_value_roundtrip_and_malformed() {
        let b = encode_reg_set_value("\\Registry\\A", "Name", 3, &[9, 8, 7]);
        assert_eq!(
            decode_reg_set_value(&b),
            Some(("\\Registry\\A", "Name", 3, &[9u8, 8, 7][..]))
        );
        let e = encode_reg_set_value("p", "", 0, &[]);
        assert_eq!(decode_reg_set_value(&e), Some(("p", "", 0, &[][..])));
        for n in 0..b.len() {
            assert_eq!(decode_reg_set_value(&b[..n]), None, "truncated at {n}");
        }
        let mut t = b.clone();
        t.push(0);
        assert_eq!(decode_reg_set_value(&t), None);
        // data length larger than the registry allows
        let mut big = encode_reg_set_value("p", "n", 1, &[]);
        let at = big.len() - 4;
        big[at..].copy_from_slice(&((1u32 << 20) + 1).to_le_bytes());
        assert_eq!(decode_reg_set_value(&big), None);
    }

    #[test]
    fn reg_delete_value_and_rename_roundtrip() {
        let b = encode_reg_delete_value("p", "v");
        assert_eq!(decode_reg_delete_value(&b), Some(("p", "v")));
        assert_eq!(decode_reg_delete_value(&b[..b.len() - 1]), None);
        let r = encode_reg_rename_key("\\Registry\\A", "B");
        assert_eq!(decode_reg_rename_key(&r), Some(("\\Registry\\A", "B")));
        assert_eq!(decode_reg_rename_key(&r[..3]), None);
        let mut t = r.clone();
        t.push(1);
        assert_eq!(decode_reg_rename_key(&t), None);
    }

    #[test]
    fn reg_create_key_roundtrip_and_malformed() {
        for v in [false, true] {
            let b = encode_reg_create_key("p", v);
            assert_eq!(decode_reg_create_key(&b), Some(("p", v)));
        }
        let mut b = encode_reg_create_key("p", true);
        *b.last_mut().unwrap() = 2;
        assert_eq!(decode_reg_create_key(&b), None, "flag must be 0 or 1");
        assert_eq!(decode_reg_create_key(&b[..b.len() - 1]), None);
    }

    #[test]
    fn reg_changed_roundtrip_and_malformed() {
        let b = encode_reg_changed("p", true, 77);
        assert_eq!(decode_reg_changed(&b), Some(("p", true, 77)));
        let b = encode_reg_changed("p", false, u64::MAX);
        assert_eq!(decode_reg_changed(&b), Some(("p", false, u64::MAX)));
        for n in 0..b.len() {
            assert_eq!(decode_reg_changed(&b[..n]), None);
        }
        let r = encode_reg_changed_reply(true, 5);
        assert_eq!(decode_reg_changed_reply(&r), Some((true, 5)));
        assert_eq!(decode_reg_changed_reply(&r[..r.len() - 1]), None);
        let mut bad = r.clone();
        bad[8] = 7;
        assert_eq!(decode_reg_changed_reply(&bad), None);
    }

    #[test]
    fn reg_version_and_lookup_reply_roundtrip() {
        assert_eq!(
            decode_reg_version_reply(&encode_reg_version_reply(9)),
            Some(9)
        );
        assert_eq!(decode_reg_version_reply(&[0; 7]), None);
        assert_eq!(decode_reg_version_reply(&[0; 9]), None);
        for state in 0..=3u8 {
            for below in [false, true] {
                let b = encode_reg_lookup_reply(state, below, 42);
                assert_eq!(decode_reg_lookup_reply(&b), Some((state, below, 42)));
            }
        }
        let mut b = encode_reg_lookup_reply(1, false, 1);
        b[8] = 4;
        assert_eq!(decode_reg_lookup_reply(&b), None, "state out of range");
        let mut b = encode_reg_lookup_reply(1, false, 1);
        b[9] = 2;
        assert_eq!(decode_reg_lookup_reply(&b), None, "bool out of range");
        assert_eq!(decode_reg_lookup_reply(&b[..9]), None);
    }

    #[test]
    fn reg_key_reply_roundtrip() {
        let n = sample_node();
        let b = encode_reg_key_reply(Some(&n), 12);
        assert_eq!(decode_reg_key_reply(&b), Some((Some(n), 12)));
        let none = encode_reg_key_reply(None, 13);
        assert_eq!(decode_reg_key_reply(&none), Some((None, 13)));
        let empty = vfs_registry::Node::default();
        let b = encode_reg_key_reply(Some(&empty), 0);
        assert_eq!(decode_reg_key_reply(&b), Some((Some(empty), 0)));
    }

    #[test]
    fn reg_key_reply_malformed_is_none_never_panics() {
        let b = encode_reg_key_reply(Some(&sample_node()), 12);
        for n in 0..b.len() {
            assert_eq!(decode_reg_key_reply(&b[..n]), None, "truncated at {n}");
        }
        let mut t = b.clone();
        t.push(0);
        assert_eq!(decode_reg_key_reply(&t), None, "trailing bytes");
        let mut bad_tag = b.clone();
        bad_tag[8] = 9;
        assert_eq!(decode_reg_key_reply(&bad_tag), None);
        // a "no node" reply with trailing bytes
        let mut none = encode_reg_key_reply(None, 1);
        none.push(0);
        assert_eq!(decode_reg_key_reply(&none), None);
        // every single-byte corruption and a huge count must not panic
        for i in 0..b.len() {
            let mut c = b.clone();
            c[i] = 0xff;
            let _ = decode_reg_key_reply(&c);
        }
        let mut huge = encode_reg_key_reply(Some(&vfs_registry::Node::default()), 0);
        // values count sits after tag, created, volatile, last_write
        let at = 8 + 1 + 1 + 1 + 8;
        huge[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode_reg_key_reply(&huge), None);
    }

    #[test]
    fn reg_key_reply_enforces_registry_limits() {
        use vfs_registry::{Child, Node, Value};
        let long_value = Node {
            values: vec![Value {
                name: "x".repeat(16384),
                ty: 1,
                data: vec![],
            }],
            ..Node::default()
        };
        let b = encode_reg_key_reply(Some(&long_value), 1);
        assert_eq!(decode_reg_key_reply(&b), None, "value name over limit");
        let mut long_child = Node::default();
        long_child
            .children
            .insert("k".into(), ("y".repeat(256), Child::Present));
        let b = encode_reg_key_reply(Some(&long_child), 1);
        assert_eq!(decode_reg_key_reply(&b), None, "child name over limit");
        let ok_value = Node {
            values: vec![Value {
                name: "x".repeat(16383),
                ty: 1,
                data: vec![0; 1 << 20],
            }],
            ..Node::default()
        };
        let b = encode_reg_key_reply(Some(&ok_value), 1);
        assert!(decode_reg_key_reply(&b).is_some());
        let too_big = Node {
            values: vec![Value {
                name: "a".into(),
                ty: 1,
                data: vec![0; (1 << 20) + 1],
            }],
            ..Node::default()
        };
        let b = encode_reg_key_reply(Some(&too_big), 1);
        assert_eq!(decode_reg_key_reply(&b), None, "data over limit");
    }
}
