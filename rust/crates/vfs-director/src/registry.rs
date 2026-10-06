//! The director's registry overlay host: one session's copy-on-write registry layer.
//!
//! [`RegistryHost`] holds the session's [`Overlay`] in memory and keeps it saved in the
//! session's registry layer, a provider holding a single file, [`OVERLAY_FILE`]. The ring
//! opcodes 15-22 (`ring_dispatch`) are answered from it.
//!
//! **Concurrency.** The overlay sits behind an `RwLock`: reads (`REG_LOOKUP`, `REG_KEY`,
//! `REG_CHANGED`) run on several ring workers at once, writes are serialised. Every answer is
//! computed under one lock acquisition together with the version it reports, so a reply never
//! pairs one state's data with another state's version.
//!
//! **Saving (plan ruling R2).** A background thread saves the whole non-volatile overlay at most
//! once per second while it is dirty, and [`RegistryHost::flush`] saves at once. A save encodes
//! under the read lock, then writes [`TMP_FILE`] and renames it over [`OVERLAY_FILE`] with no
//! overlay lock held, so ring workers are never blocked behind provider I/O. Saves are
//! serialised among themselves, so a later save never lands before an earlier one. How durable
//! the saved file is follows the provider's own policy (for a storage layer, the deferred fsync
//! policy and the host's sync at close).
//!
//! **Drop flushes.** Dropping the host stops and joins the saver thread, then saves once more if
//! anything is unsaved (errors are logged, there is no caller to return them to). The debounce
//! leaves up to a second of writes unsaved at any moment; a host that forgets `flush` should not
//! lose them, and a clean overlay costs nothing at drop.
//!
//! **A corrupt file** at open is renamed to `overlay.reg.corrupt-<unix seconds>`, logged at error
//! level, and the session starts from an empty overlay. If even the rename fails, `open` fails:
//! carrying on would overwrite the damaged file at the first save.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vfs_protocol::encode_reg_key_reply;
use vfs_provider::{
    map_io_err, Provider, VPath, OPEN_CREATE, OPEN_READ, OPEN_TRUNC, OPEN_WRITE, ST_BAD_REQUEST,
    ST_EXISTS, ST_IO_ERROR, ST_NOT_FOUND,
};
pub use vfs_registry::Lookup;
use vfs_registry::{path as regpath, Node, Overlay, RegError};

/// The registry layer's one file (plan ruling R3).
pub const OVERLAY_FILE: &str = "overlay.reg";
/// Written first, then renamed over [`OVERLAY_FILE`].
pub const TMP_FILE: &str = "overlay.reg.tmp";
/// A corrupt [`OVERLAY_FILE`] is moved to this prefix plus the unix time in seconds.
pub const CORRUPT_PREFIX: &str = "overlay.reg.corrupt-";
/// The background saver saves at most this often.
const SAVE_INTERVAL: Duration = Duration::from_secs(1);

/// Seconds between 1601-01-01 (FILETIME epoch) and 1970-01-01.
const FILETIME_UNIX_EPOCH: u64 = 11_644_473_600;

/// Now as a Windows FILETIME (100 ns ticks since 1601), the key last-write time.
fn filetime_now() -> u64 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_secs() + FILETIME_UNIX_EPOCH) * 10_000_000 + u64::from(d.subsec_nanos()) / 100
}

/// The ring status for an overlay error (plan ruling P2: the closest existing status). The shim
/// maps these to NT statuses: `ST_BAD_REQUEST` to `STATUS_INVALID_PARAMETER` (the spec's answer
/// for names and data over the limits), `ST_NOT_FOUND` to `STATUS_OBJECT_NAME_NOT_FOUND` and
/// `ST_EXISTS` to `STATUS_OBJECT_NAME_COLLISION`.
pub fn reg_status(e: RegError) -> i32 {
    match e {
        RegError::NameTooLong | RegError::DataTooLarge | RegError::InvalidPath => ST_BAD_REQUEST,
        RegError::NotFound => ST_NOT_FOUND,
        RegError::AlreadyExists => ST_EXISTS,
    }
}

/// A well-formed canonical key path: `\Registry` then non-empty components, none `.` or `..`,
/// no doubled or trailing separator (`vfs_registry::path::canonical` gives it back unchanged up
/// to case). The ring codec checks only length and UTF-8, so the director checks the syntax.
pub fn valid_path(p: &str) -> bool {
    match regpath::canonical(p, None) {
        Ok(c) => {
            regpath::fold(&c) == regpath::fold(p) && !p.split('\\').any(|c| c == "." || c == "..")
        }
        Err(_) => false,
    }
}

fn check_path(p: &str) -> Result<(), i32> {
    if valid_path(p) {
        Ok(())
    } else {
        Err(ST_BAD_REQUEST)
    }
}

/// The `REG_LOOKUP` wire state of a [`Lookup`].
pub fn lookup_state(l: Lookup) -> u8 {
    match l {
        Lookup::Absent => 0,
        Lookup::Present { created: false } => 1,
        Lookup::Present { created: true } => 2,
        Lookup::Tombstoned => 3,
    }
}

/// Somewhere a [`RegistryGeneration`] is published so every injected process of the session can
/// read it without a round trip: in practice the header of a ring the director serves
/// (`vfs_ipc::ring::publish_reg_generation`). The shim uses a cached registry answer only while
/// the generation it reads there is the one it read before asking.
pub trait RegistryGenSink: Send + Sync {
    /// Publish `generation`. Must never move what readers see backwards.
    fn publish_reg_generation(&self, generation: u64);
}

/// A director's registry generation and where it is published.
///
/// It starts at 1 and is bumped by every successful write of an attached [`RegistryHost`] and
/// by every attach or detach (`Director::set_registry`). It is not the overlay's own version,
/// which a newly attached layer restarts from its saved value: a generation never repeats, so
/// an answer cached under one layer can never look current under the next.
pub struct RegistryGeneration {
    generation: AtomicU64,
    /// Held weakly, as a ring holds the director; a sink that is gone is dropped at the next
    /// publish.
    sinks: Mutex<Vec<Weak<dyn RegistryGenSink>>>,
}

impl Default for RegistryGeneration {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistryGeneration {
    pub fn new() -> Self {
        RegistryGeneration {
            generation: AtomicU64::new(1),
            sinks: Mutex::new(Vec::new()),
        }
    }

    pub fn current(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Publish to `sink` now and after every change.
    pub fn add_sink(&self, sink: Weak<dyn RegistryGenSink>) {
        let mut sinks = self.sinks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = sink.upgrade() {
            s.publish_reg_generation(self.current());
            sinks.push(sink);
        }
    }

    /// Bump the generation and publish it to every sink before returning, so whoever is told
    /// of the change (a writer's reply) is told only after every reader can see it.
    pub fn changed(&self) {
        let mut sinks = self.sinks.lock().unwrap_or_else(|e| e.into_inner());
        // Bumped under the lock, so publishes reach the sinks in generation order.
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        sinks.retain(|w| match w.upgrade() {
            Some(s) => {
                s.publish_reg_generation(generation);
                true
            }
            None => false,
        });
    }
}

#[derive(Default)]
struct SaverState {
    /// A write happened since the saver last looked.
    kicked: bool,
    stop: bool,
}

struct Inner {
    overlay: RwLock<Overlay>,
    store: Arc<dyn Provider>,
    /// Serialises saves, and holds the overlay version the last good save captured.
    saved: Mutex<u64>,
    saver: Mutex<SaverState>,
    wake: Condvar,
}

/// One session's registry overlay, with its saver thread. See the module docs.
pub struct RegistryHost {
    inner: Arc<Inner>,
    thread: Mutex<Option<JoinHandle<()>>>,
    /// The generation every successful write moves, set by the director the host is attached
    /// to ([`RegistryHost::publish_to`]).
    generation: Mutex<Option<Arc<RegistryGeneration>>>,
}

fn lock<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>, i32> {
    m.lock().map_err(|_| map_io_err())
}

impl Inner {
    fn read(&self) -> Result<RwLockReadGuard<'_, Overlay>, i32> {
        self.overlay.read().map_err(|_| map_io_err())
    }

    fn write(&self) -> Result<RwLockWriteGuard<'_, Overlay>, i32> {
        self.overlay.write().map_err(|_| map_io_err())
    }

    /// Save if the overlay changed since the last good save. Encodes under the read lock, writes
    /// with no overlay lock held. Concurrent saves queue on `saved`.
    fn save_if_dirty(&self) -> Result<(), i32> {
        let mut saved = lock(&self.saved)?;
        let (bytes, version) = {
            let o = self.read()?;
            if o.version() == *saved {
                return Ok(());
            }
            (vfs_registry::encode(&o), o.version())
        };
        write_whole(&*self.store, TMP_FILE, &bytes)?;
        self.store
            .rename(VPath::at_default(TMP_FILE), VPath::at_default(OVERLAY_FILE))?;
        *saved = version;
        Ok(())
    }

    fn kick(&self) {
        if let Ok(mut s) = self.saver.lock() {
            s.kicked = true;
            self.wake.notify_all();
        }
    }

    /// The saver thread: waits for a write, saves, then lets at least [`SAVE_INTERVAL`] pass
    /// before the next save. A failed save is logged and retried after the interval.
    fn run_saver(&self) {
        let mut last: Option<Instant> = None;
        loop {
            let Ok(mut s) = self.saver.lock() else { return };
            loop {
                if s.stop {
                    return;
                }
                let due = last.map(|t| t + SAVE_INTERVAL);
                match due {
                    Some(due) if s.kicked && Instant::now() < due => {
                        let wait = due - Instant::now();
                        s = match self.wake.wait_timeout(s, wait) {
                            Ok((g, _)) => g,
                            Err(_) => return,
                        };
                    }
                    _ if s.kicked => break,
                    _ => {
                        s = match self.wake.wait(s) {
                            Ok(g) => g,
                            Err(_) => return,
                        };
                    }
                }
            }
            s.kicked = false;
            drop(s);
            last = Some(Instant::now());
            if let Err(st) = self.save_if_dirty() {
                tracing::error!(
                    status = st,
                    "registry overlay: background save failed; retrying"
                );
                self.kick();
            }
        }
    }
}

/// Create or truncate `name` and write all of `bytes` to it.
fn write_whole(store: &dyn Provider, name: &str, bytes: &[u8]) -> Result<(), i32> {
    let (h, _, _) = store.open(
        VPath::at_default(name),
        OPEN_WRITE | OPEN_CREATE | OPEN_TRUNC,
    )?;
    let mut off = 0usize;
    let res = (|| {
        while off < bytes.len() {
            let n = store.write_at(h, off as u64, &bytes[off..])?;
            if n == 0 {
                return Err(ST_IO_ERROR);
            }
            off += n;
        }
        Ok(())
    })();
    let closed = store.close(h);
    res.and(closed)
}

/// The whole of `name`, or `None` if it does not exist.
fn read_whole(store: &dyn Provider, name: &str) -> Result<Option<Vec<u8>>, i32> {
    let (h, size, _) = match store.open(VPath::at_default(name), OPEN_READ) {
        Ok(x) => x,
        Err(st) if st == ST_NOT_FOUND => return Ok(None),
        Err(st) => return Err(st),
    };
    let mut out = Vec::with_capacity(size.min(64 << 20) as usize);
    let mut buf = vec![0u8; 64 * 1024];
    let res = loop {
        match store.read_at(h, out.len() as u64, &mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(st) => break Err(st),
        }
    };
    let closed = store.close(h);
    res.and(closed)?;
    Ok(Some(out))
}

impl RegistryHost {
    /// Loads [`OVERLAY_FILE`] from `store`: an absent file is an empty overlay; a corrupt one is
    /// renamed to `overlay.reg.corrupt-<unix time>`, logged at error level, and an empty overlay
    /// is used. Starts the saver thread.
    pub fn open(store: Arc<dyn Provider>) -> Result<Arc<Self>, i32> {
        let overlay = match read_whole(&*store, OVERLAY_FILE)? {
            None => Overlay::new(),
            Some(bytes) => match vfs_registry::decode(&bytes) {
                Ok(o) => o,
                Err(e) => {
                    let secs = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let aside = format!("{CORRUPT_PREFIX}{secs}");
                    store
                        .rename(VPath::at_default(OVERLAY_FILE), VPath::at_default(&aside))
                        .inspect_err(|st| {
                            tracing::error!(
                                error = %e,
                                status = *st,
                                "registry overlay file is corrupt and could not be moved aside"
                            )
                        })?;
                    tracing::error!(
                        error = %e,
                        moved_to = %aside,
                        "registry overlay file is corrupt; moved aside, starting from an empty overlay"
                    );
                    Overlay::new()
                }
            },
        };
        let saved = overlay.version();
        let inner = Arc::new(Inner {
            overlay: RwLock::new(overlay),
            store,
            saved: Mutex::new(saved),
            saver: Mutex::new(SaverState::default()),
            wake: Condvar::new(),
        });
        let worker = inner.clone();
        let thread = std::thread::Builder::new()
            .name("vfs-registry-saver".into())
            .spawn(move || worker.run_saver())
            .map_err(|_| map_io_err())?;
        Ok(Arc::new(RegistryHost {
            inner,
            thread: Mutex::new(Some(thread)),
            generation: Mutex::new(None),
        }))
    }

    /// Save now if anything is unsaved (write [`TMP_FILE`], rename it over [`OVERLAY_FILE`]),
    /// and wait for it. Waits for a save already in progress first.
    pub fn flush(&self) -> Result<(), i32> {
        self.inner.save_if_dirty()
    }

    /// From now on every successful write bumps and publishes `generation` after it is applied
    /// and before it returns. The director calls this when it attaches the host.
    pub fn publish_to(&self, generation: Arc<RegistryGeneration>) {
        *self.generation.lock().unwrap_or_else(|e| e.into_inner()) = Some(generation);
    }

    /// The overlay version: bumped once by every successful write.
    pub fn version(&self) -> Result<u64, i32> {
        Ok(self.inner.read()?.version())
    }

    /// `REG_LOOKUP`: the key's state, whether the overlay holds anything below it, the version.
    pub fn lookup(&self, path: &str) -> Result<(Lookup, bool, u64), i32> {
        check_path(path)?;
        let o = self.inner.read()?;
        let (l, below) = o.lookup(path);
        Ok((l, below, o.version()))
    }

    /// `REG_KEY`: a copy of the key's overlay node (if any) and the version.
    pub fn key(&self, path: &str) -> Result<(Option<Node>, u64), i32> {
        check_path(path)?;
        let o = self.inner.read()?;
        Ok((o.node(path).cloned(), o.version()))
    }

    /// `REG_KEY`, already encoded as its ring reply (no copy of the node).
    pub fn key_reply(&self, path: &str) -> Result<Vec<u8>, i32> {
        check_path(path)?;
        let o = self.inner.read()?;
        Ok(encode_reg_key_reply(o.node(path), o.version()))
    }

    /// `REG_CHANGED`: whether the key (and, with `subtree`, anything below it) changed after
    /// `since`, and the current version.
    pub fn changed(&self, path: &str, subtree: bool, since: u64) -> Result<(bool, u64), i32> {
        check_path(path)?;
        let o = self.inner.read()?;
        Ok((o.changed_since(path, subtree, since), o.version()))
    }

    fn mutate(
        &self,
        path: &str,
        f: impl FnOnce(&mut Overlay, u64) -> Result<u64, RegError>,
    ) -> Result<u64, i32> {
        check_path(path)?;
        let v = {
            let mut o = self.inner.write()?;
            f(&mut o, filetime_now()).map_err(reg_status)?
        };
        // Applied; published before the caller (and through it the writer) hears of it.
        let generation = self
            .generation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(g) = generation {
            g.changed();
        }
        self.inner.kick();
        Ok(v)
    }

    pub fn set_value(&self, path: &str, name: &str, ty: u32, data: &[u8]) -> Result<u64, i32> {
        self.mutate(path, |o, now| o.set_value(path, name, ty, data, now))
    }

    pub fn delete_value(&self, path: &str, name: &str) -> Result<u64, i32> {
        self.mutate(path, |o, now| o.delete_value(path, name, now))
    }

    /// Creates the key as *created here*: the shim sends `REG_CREATE_KEY` only for a key with no
    /// real counterpart (or a tombstoned one), so a real key never shows through it.
    pub fn create_key(&self, path: &str, volatile: bool) -> Result<u64, i32> {
        self.mutate(path, |o, now| o.create_key(path, volatile, false, now))
    }

    pub fn delete_key(&self, path: &str) -> Result<u64, i32> {
        self.mutate(path, |o, now| o.delete_key(path, now))
    }

    pub fn rename_key(&self, path: &str, new_leaf: &str) -> Result<u64, i32> {
        if new_leaf == "." || new_leaf == ".." {
            return Err(ST_BAD_REQUEST);
        }
        self.mutate(path, |o, now| o.rename_key(path, new_leaf, now))
    }

    /// Stop and join the saver thread. Idempotent.
    fn stop_saver(&self) {
        if let Ok(mut s) = self.inner.saver.lock() {
            s.stop = true;
            self.inner.wake.notify_all();
        }
        let t = self.thread.lock().ok().and_then(|mut t| t.take());
        if let Some(t) = t {
            let _ = t.join();
        }
    }
}

impl Drop for RegistryHost {
    fn drop(&mut self) {
        self.stop_saver();
        if let Err(st) = self.inner.save_if_dirty() {
            tracing::error!(status = st, "registry overlay: final save at drop failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use vfs_provider::{Provider, RwMemFixture, VPath, OPEN_READ, ST_NOT_FOUND};

    const K: &str = r"\Registry\Machine\Software\Mod";

    fn mem() -> Arc<dyn Provider> {
        Arc::new(RwMemFixture::new())
    }

    fn read_file(p: &Arc<dyn Provider>, name: &str) -> Option<Vec<u8>> {
        let (h, _, _) = match p.open(VPath::at_default(name), OPEN_READ) {
            Ok(x) => x,
            Err(st) if st == ST_NOT_FOUND => return None,
            Err(st) => panic!("open {name}: {st}"),
        };
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = p.read_at(h, out.len() as u64, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        p.close(h).unwrap();
        Some(out)
    }

    fn names(p: &Arc<dyn Provider>) -> Vec<String> {
        p.readdir(VPath::at_default(""))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect()
    }

    #[test]
    fn absent_file_opens_empty() {
        let p = mem();
        let h = RegistryHost::open(p.clone()).unwrap();
        assert_eq!(h.lookup(K).unwrap(), (Lookup::Absent, false, 0));
        assert_eq!(h.key(K).unwrap(), (None, 0));
        // Nothing written, nothing to save.
        h.flush().unwrap();
        assert!(read_file(&p, OVERLAY_FILE).is_none());
    }

    #[test]
    fn write_flush_reopen_round_trips() {
        let p = mem();
        let h = RegistryHost::open(p.clone()).unwrap();
        h.set_value(K, "Name", 1, b"v\0").unwrap();
        h.create_key(&format!(r"{K}\Volatile"), true).unwrap();
        h.create_key(&format!(r"{K}\Kept"), false).unwrap();
        h.delete_key(&format!(r"{K}\Gone")).unwrap();
        h.flush().unwrap();
        let bytes = read_file(&p, OVERLAY_FILE).expect("saved");
        assert!(read_file(&p, TMP_FILE).is_none(), "tmp renamed away");
        drop(h);
        assert_eq!(read_file(&p, OVERLAY_FILE).unwrap(), bytes);

        let h = RegistryHost::open(p.clone()).unwrap();
        let (node, _) = h.key(K).unwrap();
        let node = node.expect("node persisted");
        assert_eq!(node.values[0].name, "Name");
        assert_eq!(node.values[0].data, b"v\0");
        assert_eq!(
            h.lookup(&format!(r"{K}\Kept")).unwrap().0,
            Lookup::Present { created: true }
        );
        // Volatile keys are never saved.
        assert_eq!(
            h.lookup(&format!(r"{K}\Volatile")).unwrap().0,
            Lookup::Absent
        );
        assert_eq!(
            h.lookup(&format!(r"{K}\Gone")).unwrap().0,
            Lookup::Tombstoned
        );
    }

    #[test]
    fn corrupt_file_is_moved_aside_and_overlay_is_empty() {
        let p = mem();
        let (fh, _, _) = p
            .open(
                VPath::at_default(OVERLAY_FILE),
                vfs_provider::OPEN_WRITE | vfs_provider::OPEN_CREATE,
            )
            .unwrap();
        p.write_at(fh, 0, b"definitely not an overlay").unwrap();
        p.close(fh).unwrap();

        let h = RegistryHost::open(p.clone()).unwrap();
        assert_eq!(h.lookup(K).unwrap(), (Lookup::Absent, false, 0));
        assert!(read_file(&p, OVERLAY_FILE).is_none(), "moved away");
        let aside: Vec<String> = names(&p)
            .into_iter()
            .filter(|n| n.starts_with("overlay.reg.corrupt-"))
            .collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        let secs: u64 = aside[0]["overlay.reg.corrupt-".len()..].parse().unwrap();
        assert!(secs > 1_700_000_000, "unix time: {secs}");
        assert_eq!(
            read_file(&p, &aside[0]).unwrap(),
            b"definitely not an overlay"
        );
        // The empty overlay works and saves normally.
        h.set_value(K, "v", 4, &1u32.to_le_bytes()).unwrap();
        h.flush().unwrap();
        assert!(read_file(&p, OVERLAY_FILE).is_some());
    }

    #[test]
    fn background_saver_saves_without_flush() {
        let p = mem();
        let h = RegistryHost::open(p.clone()).unwrap();
        h.set_value(K, "v", 1, b"x\0").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while read_file(&p, OVERLAY_FILE).is_none() {
            assert!(Instant::now() < deadline, "saver never saved");
            std::thread::sleep(Duration::from_millis(20));
        }
        let o = vfs_registry::decode(&read_file(&p, OVERLAY_FILE).unwrap()).unwrap();
        assert!(o.node(K).is_some());
        drop(h);
    }

    /// Counts completed saves (renames onto `overlay.reg`).
    struct Counting {
        inner: RwMemFixture,
        saves: std::sync::atomic::AtomicUsize,
    }

    impl Provider for Counting {
        fn capabilities(&self) -> vfs_provider::Capabilities {
            self.inner.capabilities()
        }
        fn getattr(&self, p: VPath) -> Result<Option<vfs_provider::Stat>, i32> {
            self.inner.getattr(p)
        }
        fn readdir(&self, p: VPath) -> Result<Vec<vfs_provider::DirEntry>, i32> {
            self.inner.readdir(p)
        }
        fn open(&self, p: VPath, f: u32) -> Result<(vfs_provider::Handle, u64, bool), i32> {
            self.inner.open(p, f)
        }
        fn close(&self, h: vfs_provider::Handle) -> Result<(), i32> {
            self.inner.close(h)
        }
        fn read_at(&self, h: vfs_provider::Handle, o: u64, b: &mut [u8]) -> Result<usize, i32> {
            self.inner.read_at(h, o, b)
        }
        fn write_at(&self, h: vfs_provider::Handle, o: u64, b: &[u8]) -> Result<usize, i32> {
            self.inner.write_at(h, o, b)
        }
        fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
            let r = self.inner.rename(from, to);
            if r.is_ok() && to.rel == OVERLAY_FILE {
                self.saves.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            r
        }
    }

    #[test]
    fn saver_is_debounced_to_once_per_second() {
        let c = Arc::new(Counting {
            inner: RwMemFixture::new(),
            saves: Default::default(),
        });
        let h = RegistryHost::open(c.clone()).unwrap();
        let start = Instant::now();
        let mut i = 0u32;
        while start.elapsed() < Duration::from_millis(1500) {
            h.set_value(K, "n", 4, &i.to_le_bytes()).unwrap();
            i += 1;
            std::thread::sleep(Duration::from_millis(2));
        }
        let during = c.saves.load(std::sync::atomic::Ordering::SeqCst);
        // 1.5 s of continuous writes: the first save, then at most one more per second.
        assert!((1..=3).contains(&during), "saves during 1.5 s: {during}");
        h.flush().unwrap();
        let o = vfs_registry::decode(
            &read_file(&(c.clone() as Arc<dyn Provider>), OVERLAY_FILE).unwrap(),
        )
        .unwrap();
        assert_eq!(o.node(K).unwrap().values[0].data, (i - 1).to_le_bytes());
        // A clean flush writes nothing.
        let before = c.saves.load(std::sync::atomic::Ordering::SeqCst);
        h.flush().unwrap();
        assert_eq!(c.saves.load(std::sync::atomic::Ordering::SeqCst), before);
    }

    #[test]
    fn drop_stops_the_saver_and_flushes() {
        let p = mem();
        let h = RegistryHost::open(p.clone()).unwrap();
        h.set_value(K, "late", 1, b"z\0").unwrap();
        drop(h);
        // The saver thread held the store; after drop nothing does.
        assert_eq!(Arc::strong_count(&p), 1, "saver thread still alive");
        let o =
            vfs_registry::decode(&read_file(&p, OVERLAY_FILE).expect("flushed on drop")).unwrap();
        assert_eq!(o.node(K).unwrap().values[0].name, "late");
    }

    #[test]
    fn invalid_paths_are_bad_request() {
        let h = RegistryHost::open(mem()).unwrap();
        for bad in [
            "",
            r"Registry\Machine",
            r"\Device\X",
            r"\Registry\Machine\\X",
            r"\Registry\Machine\X\",
            r"\Registry\Machine\..\User",
            r"\Registry\Machine\.",
        ] {
            assert_eq!(h.lookup(bad).err(), Some(ST_BAD_REQUEST), "{bad:?}");
            assert_eq!(
                h.set_value(bad, "v", 1, b"").err(),
                Some(ST_BAD_REQUEST),
                "{bad:?}"
            );
        }
        assert_eq!(h.rename_key(K, "..").err(), Some(ST_BAD_REQUEST));
    }
}
