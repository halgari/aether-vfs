//! The shim's registry client: the registry hooks' one way to the director's registry overlay
//! (ring opcodes 15-22, spec section 4).
//!
//! **Caching (spec section 3.5).** `REG_LOOKUP` and `REG_KEY` answers are cached per path. The
//! director publishes a registry *generation* in the ring header that every injected process of
//! the session maps, and moves it on every registry write (before the writer gets its reply)
//! and whenever a layer is attached or detached (`vfs_director::Director::registry_changed`).
//! An answer is cached under the generation read **before** it was asked for, and used only
//! while the published generation still equals it. So a write by any process makes every
//! process's cached answers unusable before that write returns, and a cache hit costs one
//! atomic load and no round trip.
//!
//! An answer may be newer than the generation it is cached under (a write landed between the
//! read of the generation and the director's answer). That is harmless: the write moves the
//! generation, so the entry is never used past it.
//!
//! **Errors (spec section 6).** Every call returns the director's status as `Err`: a request
//! that could not be sent or was never answered is `ST_IO_ERROR`, a director with no registry
//! attached `ST_NOT_SUPPORTED`, a `REG_KEY` answer too large for the ring
//! `ST_REPLY_TOO_LARGE`. A failed read is counted as a fallback in the shim stats; the caller
//! then serves the real key alone. A failed write is never retried against the real registry;
//! the caller returns `STATUS_UNSUCCESSFUL`.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Mutex, OnceLock};

use vfs_protocol::{
    decode_reg_changed_reply, decode_reg_key_reply, decode_reg_lookup_reply,
    decode_reg_version_reply, encode_reg_changed, encode_reg_create_key, encode_reg_delete_value,
    encode_reg_path, encode_reg_rename_key, encode_reg_set_value, OP_REG_CHANGED,
    OP_REG_CREATE_KEY, OP_REG_DELETE_KEY, OP_REG_DELETE_VALUE, OP_REG_KEY, OP_REG_LOOKUP,
    OP_REG_RENAME_KEY, OP_REG_SET_VALUE, ST_BAD_REQUEST, ST_NOT_SUPPORTED,
};
use vfs_registry::{path::fold, Lookup, Node};

use crate::fuse_client::{self, FuseClient};

/// Entries per map before the cache starts over. A game reads far fewer distinct keys than
/// this between two writes; the bound only stops a pathological enumeration from growing it
/// without end.
const CACHE_ENTRIES: usize = 4096;

/// Registry virtualisation is on for this process: the host set [`vfs_env::REGISTRY`] (it does
/// so only while a registry layer is attached) and the shim has a director to ask.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| vfs_env::opt_in(vfs_env::REGISTRY)) && fuse_client::global().is_some()
}

/// The process's registry client, over [`fuse_client::global`].
pub fn global() -> Option<&'static RegClient<'static>> {
    static CLIENT: OnceLock<RegClient<'static>> = OnceLock::new();
    let fc = fuse_client::global()?;
    Some(CLIENT.get_or_init(|| RegClient::new(fc)))
}

/// `REG_LOOKUP` through [`global`]: the key's overlay state and whether the overlay holds
/// anything below it. Cached.
pub fn lookup(path: &str) -> Result<(Lookup, bool), i32> {
    match global() {
        Some(c) => c.lookup(path),
        None => read_failed(ST_NOT_SUPPORTED),
    }
}

/// `REG_KEY` through [`global`]: the key's overlay node, if it has one. Cached.
pub fn key(path: &str) -> Result<Option<Node>, i32> {
    match global() {
        Some(c) => c.key(path),
        None => read_failed(ST_NOT_SUPPORTED),
    }
}

pub fn set_value(path: &str, name: &str, ty: u32, data: &[u8]) -> Result<(), i32> {
    global()
        .ok_or(ST_NOT_SUPPORTED)?
        .set_value(path, name, ty, data)
}

pub fn delete_value(path: &str, name: &str) -> Result<(), i32> {
    global().ok_or(ST_NOT_SUPPORTED)?.delete_value(path, name)
}

pub fn create_key(path: &str, volatile: bool) -> Result<(), i32> {
    global().ok_or(ST_NOT_SUPPORTED)?.create_key(path, volatile)
}

pub fn delete_key(path: &str) -> Result<(), i32> {
    global().ok_or(ST_NOT_SUPPORTED)?.delete_key(path)
}

pub fn rename_key(path: &str, new_leaf: &str) -> Result<(), i32> {
    global().ok_or(ST_NOT_SUPPORTED)?.rename_key(path, new_leaf)
}

/// `REG_CHANGED` through [`global`]: whether the key (and with `subtree` anything below it)
/// changed after overlay version `since`, and the current overlay version. Never cached.
pub fn changed(path: &str, subtree: bool, since: u64) -> Result<(bool, u64), i32> {
    global()
        .ok_or(ST_NOT_SUPPORTED)?
        .changed(path, subtree, since)
}

/// Count a failed read as a fallback to the real registry, and return its status.
fn read_failed<T>(status: i32) -> Result<T, i32> {
    crate::hookstats::note_reg_read_fallback();
    Err(status)
}

/// The cached answers of one registry generation.
#[derive(Default)]
struct Cache {
    generation: u64,
    lookups: HashMap<String, (Lookup, bool)>,
    keys: HashMap<String, Option<Node>>,
}

impl Cache {
    /// Bring the cache to `generation` (dropping everything when it is newer than what the
    /// cache holds) and say whether its entries are answers for that generation. A reader
    /// holding an older generation than the cache's is told no: it neither uses nor adds.
    fn at(&mut self, generation: u64) -> bool {
        if generation > self.generation {
            self.lookups.clear();
            self.keys.clear();
            self.generation = generation;
        }
        generation == self.generation
    }
}

/// A registry client over one ring: the requests, and the cache of read answers.
///
/// The process has one, [`global`]; a test makes more to play several processes.
pub struct RegClient<'a> {
    fc: &'a FuseClient,
    cache: Mutex<Cache>,
}

impl<'a> RegClient<'a> {
    pub fn new(fc: &'a FuseClient) -> Self {
        RegClient {
            fc,
            cache: Mutex::new(Cache::default()),
        }
    }

    pub fn lookup(&self, path: &str) -> Result<(Lookup, bool), i32> {
        self.cached(
            path,
            |c| &mut c.lookups,
            || {
                let r = self.fc.reg_request(OP_REG_LOOKUP, &encode_reg_path(path))?;
                let (state, below, _) = decode_reg_lookup_reply(&r).ok_or(ST_BAD_REQUEST)?;
                let l = match state {
                    0 => Lookup::Absent,
                    1 => Lookup::Present { created: false },
                    2 => Lookup::Present { created: true },
                    _ => Lookup::Tombstoned,
                };
                Ok((l, below))
            },
        )
    }

    pub fn key(&self, path: &str) -> Result<Option<Node>, i32> {
        self.cached(
            path,
            |c| &mut c.keys,
            || {
                let r = self.fc.reg_request(OP_REG_KEY, &encode_reg_path(path))?;
                Ok(decode_reg_key_reply(&r).ok_or(ST_BAD_REQUEST)?.0)
            },
        )
    }

    pub fn set_value(&self, path: &str, name: &str, ty: u32, data: &[u8]) -> Result<(), i32> {
        self.write(
            OP_REG_SET_VALUE,
            &encode_reg_set_value(path, name, ty, data),
        )
    }

    pub fn delete_value(&self, path: &str, name: &str) -> Result<(), i32> {
        self.write(OP_REG_DELETE_VALUE, &encode_reg_delete_value(path, name))
    }

    pub fn create_key(&self, path: &str, volatile: bool) -> Result<(), i32> {
        self.write(OP_REG_CREATE_KEY, &encode_reg_create_key(path, volatile))
    }

    pub fn delete_key(&self, path: &str) -> Result<(), i32> {
        self.write(OP_REG_DELETE_KEY, &encode_reg_path(path))
    }

    pub fn rename_key(&self, path: &str, new_leaf: &str) -> Result<(), i32> {
        self.write(OP_REG_RENAME_KEY, &encode_reg_rename_key(path, new_leaf))
    }

    pub fn changed(&self, path: &str, subtree: bool, since: u64) -> Result<(bool, u64), i32> {
        let r = self
            .fc
            .reg_request(OP_REG_CHANGED, &encode_reg_changed(path, subtree, since))?;
        decode_reg_changed_reply(&r).ok_or(ST_BAD_REQUEST)
    }

    /// A write. Nothing to invalidate here: the director moves the published generation
    /// before it replies, which makes every cached answer, this client's included, unusable.
    fn write(&self, opcode: u32, payload: &[u8]) -> Result<(), i32> {
        let r = self.fc.reg_request(opcode, payload)?;
        decode_reg_version_reply(&r).ok_or(ST_BAD_REQUEST)?;
        Ok(())
    }

    /// A read answered from `map` while the published generation is the one the answer was
    /// cached under, otherwise by `fetch` (and cached). Generation 0 means the director
    /// published nothing, so nothing is cached.
    fn cached<T: Clone>(
        &self,
        path: &str,
        map: fn(&mut Cache) -> &mut HashMap<String, T>,
        fetch: impl FnOnce() -> Result<T, i32>,
    ) -> Result<T, i32> {
        let generation = self.fc.reg_generation();
        let k = fold(path);
        if generation != 0 {
            if let Ok(mut c) = self.cache.lock() {
                if c.at(generation) {
                    if let Some(v) = map(&mut c).get(&k) {
                        return Ok(v.clone());
                    }
                }
            }
        }
        let v = match fetch() {
            Ok(v) => v,
            Err(st) => return read_failed(st),
        };
        if generation != 0 {
            if let Ok(mut c) = self.cache.lock() {
                if c.at(generation) {
                    insert_bounded(map(&mut c), k, v.clone());
                }
            }
        }
        Ok(v)
    }
}

fn insert_bounded<K: Eq + Hash, V>(m: &mut HashMap<K, V>, k: K, v: V) {
    if m.len() >= CACHE_ENTRIES {
        m.clear();
    }
    m.insert(k, v);
}
