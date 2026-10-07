use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use crate::{InlineProvider, MemoryProvider};
use vfs_core::fold;
use vfs_provider::{map_io_err, CaseMatch, KIND_FILE, OPEN_CREATE, OPEN_READ};

/// Slow and immutable, but sequential-only — exercises both the
/// pass-through fields and the forced access/immutable overrides at once.
struct SlowSeqBase;

impl Provider for SlowSeqBase {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            access: vfs_provider::Access::SeqRead,
            immutable: true,
            slow: true,
            preferred_block: Some(4096),
            // Never resolves any name (see the stubs below), so
            // fold-equal-resolves-identically holds vacuously.
            case: CaseMatch::Insensitive,
        }
    }
    fn getattr(&self, _p: VPath) -> Result<Option<Stat>, i32> {
        Ok(None)
    }
    fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
        Ok(Vec::new())
    }
    fn open(&self, _p: VPath, _f: u32) -> Result<(Handle, u64, bool), i32> {
        Err(not_found())
    }
    fn close(&self, _h: Handle) -> Result<(), i32> {
        Ok(())
    }
    fn read_at(&self, _h: Handle, _o: u64, _b: &mut [u8]) -> Result<usize, i32> {
        Ok(0)
    }
}

/// An empty in-memory `ReadWrite` upper that matches names **byte-exactly**
/// and says so (`CaseMatch::Sensitive`), like a case-sensitive disk. It wraps
/// `MemoryProvider`, which folds, by escaping every character that folding
/// would change (`~<hex>~`) on the way in and undoing that on the way out, so
/// two spellings of one name are two entries. The overlay tests run on this
/// so the overlay's behaviour over a case-sensitive upper stays covered;
/// `MemoryProvider` itself, a case-insensitive upper, is used where a test
/// wants that (`stored_name`).
#[derive(Default)]
struct ExactUpper {
    inner: MemoryProvider,
}

fn escape_exact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let one = c.to_string();
        if c == '~' || fold(&one) != one {
            out.push_str(&format!("~{:x}~", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

fn unescape_exact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('~') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        let end = tail.find('~').expect("escaped name has a closing ~");
        let code = u32::from_str_radix(&tail[..end], 16).expect("hex code point");
        out.push(char::from_u32(code).expect("valid char"));
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

impl Provider for ExactUpper {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            case: CaseMatch::Sensitive,
            ..self.inner.capabilities()
        }
    }
    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.inner.getattr(VPath::new(p.root, &escape_exact(p.rel)))
    }
    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let mut entries = self
            .inner
            .readdir(VPath::new(p.root, &escape_exact(p.rel)))?;
        for e in &mut entries {
            e.name = unescape_exact(&e.name);
        }
        Ok(entries)
    }
    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        self.inner
            .open(VPath::new(p.root, &escape_exact(p.rel)), flags)
    }
    fn close(&self, h: Handle) -> Result<(), i32> {
        self.inner.close(h)
    }
    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        self.inner.read_at(h, offset, buf)
    }
    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        self.inner.write_at(h, offset, buf)
    }
    fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
        self.inner.set_len(h, len)
    }
    fn flush(&self, h: Handle) -> Result<(), i32> {
        self.inner.flush(h)
    }
    fn mkdir(&self, p: VPath) -> Result<(), i32> {
        self.inner.mkdir(VPath::new(p.root, &escape_exact(p.rel)))
    }
    fn remove(&self, p: VPath) -> Result<(), i32> {
        self.inner.remove(VPath::new(p.root, &escape_exact(p.rel)))
    }
    fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
        self.inner.rename(
            VPath::new(from.root, &escape_exact(from.rel)),
            VPath::new(to.root, &escape_exact(to.rel)),
        )
    }
    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        self.inner
            .set_attr(VPath::new(p.root, &escape_exact(p.rel)), attr)
    }
}

/// Wraps a `Provider` and counts calls to `open`, so a test can assert a
/// piece of code touched the wrapped provider exactly N times. A
/// final-size or final-content check on a copy-up race can't distinguish
/// "one thread copied" from "eight threads copied the same bytes" — this
/// can.
struct CountingOpens<P> {
    inner: P,
    opens: AtomicU64,
}

impl<P> CountingOpens<P> {
    fn new(inner: P) -> Self {
        CountingOpens {
            inner,
            opens: AtomicU64::new(0),
        }
    }
}

impl<P: Provider> Provider for CountingOpens<P> {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.inner.getattr(p)
    }
    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.inner.readdir(p)
    }
    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        self.opens.fetch_add(1, Ordering::Relaxed);
        self.inner.open(p, flags)
    }
    fn close(&self, h: Handle) -> Result<(), i32> {
        self.inner.close(h)
    }
    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        self.inner.read_at(h, offset, buf)
    }
}

/// Counts the calls an overlay makes into its **upper**, which is where
/// the whiteout bookkeeping lands. A correctness test cannot see the cost
/// of that bookkeeping at all — the answers are identical either way —
/// so this is the only thing that can hold the read path to a budget.
#[derive(Default)]
struct CountingUpper {
    inner: ExactUpper,
    getattrs: AtomicU64,
    readdirs: AtomicU64,
    /// Runs once, after the next `readdir` has read the directory and
    /// before it returns: what another thread could do in that window.
    #[allow(clippy::type_complexity)]
    after_readdir: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Provider for CountingUpper {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.getattrs.fetch_add(1, Ordering::Relaxed);
        self.inner.getattr(p)
    }
    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.readdirs.fetch_add(1, Ordering::Relaxed);
        let entries = self.inner.readdir(p);
        let hook = self.after_readdir.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        entries
    }
    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        self.inner.open(p, flags)
    }
    fn close(&self, h: Handle) -> Result<(), i32> {
        self.inner.close(h)
    }
    fn read_at(&self, h: Handle, o: u64, b: &mut [u8]) -> Result<usize, i32> {
        self.inner.read_at(h, o, b)
    }
    fn write_at(&self, h: Handle, o: u64, b: &[u8]) -> Result<usize, i32> {
        self.inner.write_at(h, o, b)
    }
    fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
        self.inner.set_len(h, len)
    }
    fn flush(&self, h: Handle) -> Result<(), i32> {
        self.inner.flush(h)
    }
    fn mkdir(&self, p: VPath) -> Result<(), i32> {
        self.inner.mkdir(p)
    }
    fn remove(&self, p: VPath) -> Result<(), i32> {
        self.inner.remove(p)
    }
    fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
        self.inner.rename(from, to)
    }
    fn set_attr(&self, p: VPath, a: SetAttr) -> Result<(), i32> {
        self.inner.set_attr(p, a)
    }
}

/// A base whose `read_at` succeeds for the first chunk of a file and then
/// fails every call after — used to prove that a copy-up which dies
/// partway through a multi-chunk file leaves no trace in upper, rather
/// than a truncated destination that a later check would mistake for a
/// complete copy.
struct FlakyReadBase {
    body: Vec<u8>,
}

impl Provider for FlakyReadBase {
    fn capabilities(&self) -> Capabilities {
        // getattr below compares `p.rel` to "big.bin" by byte equality.
        Capabilities {
            case: CaseMatch::Sensitive,
            ..Capabilities::read_only()
        }
    }
    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        if p.rel.is_empty() {
            return Ok(Some(Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: 0,
            }));
        }
        if p.rel == "big.bin" {
            return Ok(Some(Stat {
                kind: KIND_FILE,
                size: self.body.len() as u64,
                mtime: 0,
            }));
        }
        Ok(None)
    }
    fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
        Ok(vec![DirEntry {
            name: "big.bin".to_string(),
            stat: Stat {
                kind: KIND_FILE,
                size: self.body.len() as u64,
                mtime: 0,
            },
        }])
    }
    fn open(&self, p: VPath, _flags: u32) -> Result<(Handle, u64, bool), i32> {
        if p.rel == "big.bin" {
            Ok((1, self.body.len() as u64, false))
        } else {
            Err(not_found())
        }
    }
    fn close(&self, _h: Handle) -> Result<(), i32> {
        Ok(())
    }
    fn read_at(&self, _h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        // First chunk (copy_loop's buffer is 64 KiB) succeeds; anything
        // after that fails, simulating a read that dies partway through
        // a multi-chunk file.
        if offset >= 65536 {
            return Err(map_io_err());
        }
        let start = offset as usize;
        let n = (self.body.len() - start).min(buf.len());
        buf[..n].copy_from_slice(&self.body[start..start + n]);
        Ok(n)
    }
}

#[test]
fn stored_name_takes_the_bases_spelling_then_the_uppers_and_honours_whiteouts() {
    use crate::MemoryProvider;
    let base = Arc::new(InlineProvider::from_files([(
        "Data/Skyrim.esm",
        b"B".as_slice(),
    )]));
    let upper = MemoryProvider::from_files([
        ("data/skyrim.esm", b"U".as_slice()),
        ("data/New.ESP", b"N".as_slice()),
    ]);
    let ov = OverlayProvider::new(base, upper).unwrap();
    let name = |q: &str| {
        ov.stored_name(VPath::at_default(q))
            .expect("answered, not unsupported")
    };

    // A name both sides have keeps the base's spelling, for any query case.
    assert_eq!(name("data/SKYRIM.ESM").as_deref(), Some("Skyrim.esm"));
    assert_eq!(name("DATA").as_deref(), Some("Data"));
    // A name only the upper has is spelled as the upper spells it.
    assert_eq!(name("Data/new.esp").as_deref(), Some("New.ESP"));
    assert_eq!(name("data/nope"), None);
    assert_eq!(name(""), None);
    // And it agrees with the listing.
    let listed: Vec<String> = ov
        .readdir(VPath::at_default("data"))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(listed.contains(&"Skyrim.esm".to_string()) && listed.contains(&"New.ESP".to_string()));

    // A whiteout hides the name from `stored_name` as it does from the listing.
    ov.remove(VPath::at_default("data/skyrim.esm")).unwrap();
    assert_eq!(name("data/skyrim.esm"), None);
}

#[test]
fn overlay_reports_read_write_and_is_never_immutable() {
    let ov = OverlayProvider::new(Arc::new(SlowSeqBase), ExactUpper::default()).unwrap();
    let caps = ov.capabilities();
    assert_eq!(
        caps.access,
        vfs_provider::Access::ReadWrite,
        "a writable upper makes the whole stack writable regardless of the base"
    );
    // A stack you can write to is by definition not immutable: claiming
    // otherwise would be a promise a caching layer would act on, and
    // Capabilities::validate() rejects ReadWrite + immutable as the
    // self-contradiction it is. Do not "fix" this back to true.
    assert!(!caps.immutable, "a writable stack can never be immutable");
    assert!(caps.slow, "slow still derives from the children");
    assert_eq!(caps.preferred_block, Some(4096));
}

#[test]
fn overlay_over_the_fixture_tree_with_an_empty_upper_passes_conformance() {
    let base: Arc<dyn Provider> = Arc::new(InlineProvider::from_files(
        vfs_provider::FIXTURE_FILES.iter().copied(),
    ));
    let p: Arc<dyn Provider> = Arc::new(OverlayProvider::new(base, ExactUpper::default()).unwrap());
    vfs_provider::assert_conformance(p);
}

#[test]
fn upper_wins_and_whiteout_hides() {
    let base = Arc::new(InlineProvider::from_files([
        ("a.txt", b"BASE".as_slice()),
        ("gone.txt", b"X".as_slice()),
    ]));
    let upper = ExactUpper::default();
    let (h, _, _) = upper
        .open(VPath::at_default("a.txt"), OPEN_WRITE | OPEN_CREATE)
        .unwrap();
    upper.write_at(h, 0, b"UPPER").unwrap();
    upper.close(h).unwrap();
    let (h, _, _) = upper
        .open(VPath::at_default(".wh.gone.txt"), OPEN_WRITE | OPEN_CREATE)
        .unwrap();
    upper.close(h).unwrap();
    let ov = OverlayProvider::new(base, upper).unwrap();

    let st = ov.getattr(VPath::at_default("a.txt")).unwrap().unwrap();
    assert_eq!(st.size, 5);
    assert!(ov.getattr(VPath::at_default("gone.txt")).unwrap().is_none());

    let (h, _, _) = ov.open(VPath::at_default("a.txt"), OPEN_READ).unwrap();
    let mut buf = [0u8; 8];
    let n = ov.read_at(h, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"UPPER");
    ov.close(h).unwrap();
}

#[test]
fn overlay_declares_read_write_over_a_read_only_base() {
    use vfs_provider::{Access, Provider};
    let base = Arc::new(InlineProvider::from_files(
        vfs_provider::FIXTURE_FILES.iter().copied(),
    ));
    let ov = OverlayProvider::new(base, ExactUpper::default()).unwrap();
    assert_eq!(ov.capabilities().access, Access::ReadWrite);
}

#[test]
fn overlay_rejects_a_read_only_upper_at_construction() {
    let base = Arc::new(InlineProvider::from_files(
        vfs_provider::FIXTURE_FILES.iter().copied(),
    ));
    let upper = InlineProvider::from_files(std::iter::empty::<(&str, &[u8])>());
    assert!(
        OverlayProvider::new(base, upper).is_err(),
        "a read-only upper must be refused at construction, not at first write"
    );
}

#[test]
fn writing_a_base_file_copies_it_up_and_leaves_base_untouched() {
    use vfs_provider::{Provider, VPath, OPEN_READ, OPEN_WRITE};
    let base = Arc::new(InlineProvider::from_files([("a.txt", b"BASE".as_slice())]));
    let ov = OverlayProvider::new(base.clone(), ExactUpper::default()).unwrap();

    let f = VPath::at_default("a.txt");
    let (h, _, _) = ov.open(f, OPEN_WRITE).expect("open for write copies up");
    ov.write_at(h, 0, b"UP").expect("write");
    ov.close(h).expect("close");

    let (h, _, _) = ov.open(f, OPEN_READ).expect("reopen");
    let mut buf = [0u8; 8];
    let n = ov.read_at(h, 0, &mut buf).expect("read");
    assert_eq!(
        &buf[..n],
        b"UPSE",
        "copy-up must preserve the untouched tail"
    );
    ov.close(h).expect("close");

    // The base is never mutated.
    let (bh, _, _) = base.open(f, OPEN_READ).expect("base open");
    let n = base.read_at(bh, 0, &mut buf).expect("base read");
    assert_eq!(&buf[..n], b"BASE", "copy-up mutated the base");
    base.close(bh).unwrap();
}

#[test]
fn removing_a_base_file_writes_a_whiteout() {
    use vfs_provider::{Provider, VPath};
    let base = Arc::new(InlineProvider::from_files([("a.txt", b"BASE".as_slice())]));
    let ov = OverlayProvider::new(base, ExactUpper::default()).unwrap();
    let f = VPath::at_default("a.txt");
    ov.remove(f).expect("remove");
    assert!(
        ov.getattr(f).expect("getattr").is_none(),
        "whiteout did not hide the base file"
    );
    assert!(
        !ov.readdir(VPath::at_default(""))
            .expect("readdir")
            .iter()
            .any(|e| e.name == "a.txt"),
        "whiteout did not hide the entry from readdir"
    );
}

#[test]
fn concurrent_opens_copy_up_exactly_once() {
    use std::sync::Arc as StdArc;
    use vfs_provider::{Provider, VPath, OPEN_WRITE};
    let counted = Arc::new(CountingOpens::new(InlineProvider::from_files([(
        "a.txt",
        b"BASE".as_slice(),
    )])));
    let base: Arc<dyn Provider> = counted.clone();
    let ov: StdArc<OverlayProvider> =
        StdArc::new(OverlayProvider::new(base, ExactUpper::default()).unwrap());

    let mut hs = Vec::new();
    for _ in 0..8 {
        let ov = StdArc::clone(&ov);
        hs.push(std::thread::spawn(move || {
            let (h, _, _) = ov
                .open(VPath::at_default("a.txt"), OPEN_WRITE)
                .expect("open");
            ov.close(h).expect("close");
        }));
    }
    for h in hs {
        h.join().expect("thread");
    }
    // Content must still be the base content, not a truncated or doubled copy.
    let (h, size, _) = ov
        .open(VPath::at_default("a.txt"), vfs_provider::OPEN_READ)
        .unwrap();
    assert_eq!(size, 4, "concurrent copy-up corrupted the file");
    ov.close(h).unwrap();

    // The assertion that actually proves exclusivity: every racing
    // thread reads/writes identical bytes, so the size check above would
    // pass just the same if all eight threads had copied concurrently.
    // Counting how many times the base was opened does not.
    assert_eq!(
        counted.opens.load(Ordering::Relaxed),
        1,
        "copy-up opened the base more than once — the in-flight lock did not serialize the race"
    );
}

/// The in-flight set sleeps its waiters rather than spinning, and a second
/// claim on a held key proceeds as soon as the holder drops its guard.
#[test]
fn a_held_copy_up_key_makes_the_next_claim_wait_until_it_is_released() {
    use super::copy_up::InFlight;
    use std::sync::mpsc;
    use std::time::Duration;

    let flight = Arc::new(InFlight::default());
    let guard = flight.claim((1, "a.txt".to_string())).unwrap();

    let (tx, rx) = mpsc::channel();
    let waiter = {
        let flight = Arc::clone(&flight);
        std::thread::spawn(move || {
            let _g = flight.claim((1, "a.txt".to_string())).unwrap();
            tx.send(()).unwrap();
        })
    };
    assert!(
        rx.recv_timeout(Duration::from_millis(150)).is_err(),
        "a second claim on a held key did not wait"
    );
    drop(guard);
    rx.recv_timeout(Duration::from_secs(10))
        .expect("the waiter was not woken when the key was released");
    waiter.join().unwrap();
}

/// The same relative path under two roots is two files: holding one must not
/// make the other wait.
#[test]
fn copy_up_keys_include_the_root() {
    use super::copy_up::InFlight;
    use std::sync::mpsc;
    use std::time::Duration;

    let flight = Arc::new(InFlight::default());
    let _held = flight.claim((1, "a.txt".to_string())).unwrap();

    let (tx, rx) = mpsc::channel();
    let other = {
        let flight = Arc::clone(&flight);
        std::thread::spawn(move || {
            let _g = flight.claim((2, "a.txt".to_string())).unwrap();
            tx.send(()).unwrap();
        })
    };
    rx.recv_timeout(Duration::from_secs(10))
        .expect("a claim under another root waited for an unrelated copy-up");
    other.join().unwrap();
}

/// A one-file base whose first `open` of `block_root` parks until the test
/// releases it, then fails if `fail` is set. Lets a test hold a copy-up
/// in flight, with the overlay's slot claimed, while another thread arrives.
struct GatedBase {
    block_root: u32,
    fail: bool,
    first: AtomicU64,
    entered: Mutex<std::sync::mpsc::Sender<()>>,
    gate: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl GatedBase {
    fn new(
        block_root: u32,
        fail: bool,
    ) -> (
        Arc<Self>,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (gate_tx, gate_rx) = std::sync::mpsc::channel();
        let base = Arc::new(GatedBase {
            block_root,
            fail,
            first: AtomicU64::new(0),
            entered: Mutex::new(entered_tx),
            gate: Mutex::new(gate_rx),
        });
        (base, entered_rx, gate_tx)
    }
}

impl Provider for GatedBase {
    fn capabilities(&self) -> Capabilities {
        Capabilities::read_only()
    }
    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        Ok((p.rel == "a.txt").then_some(Stat {
            kind: KIND_FILE,
            size: 4,
            mtime: 0,
        }))
    }
    fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
        Ok(Vec::new())
    }
    fn open(&self, p: VPath, _f: u32) -> Result<(Handle, u64, bool), i32> {
        if p.root.0 == self.block_root && self.first.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.lock().unwrap().send(()).unwrap();
            self.gate.lock().unwrap().recv().unwrap();
            if self.fail {
                return Err(map_io_err());
            }
        }
        Ok((1, 4, false))
    }
    fn close(&self, _h: Handle) -> Result<(), i32> {
        Ok(())
    }
    fn read_at(&self, _h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let body = b"BASE";
        let start = (offset as usize).min(body.len());
        let n = (body.len() - start).min(buf.len());
        buf[..n].copy_from_slice(&body[start..start + n]);
        Ok(n)
    }
}

/// A caller waiting on a copy-up that fails is woken, takes the slot, and
/// makes its own attempt; it is not left asleep and not served a half copy.
#[test]
fn a_waiter_proceeds_after_the_copy_it_waited_on_fails() {
    use std::sync::mpsc;
    use std::time::Duration;
    let (base, entered, gate) = GatedBase::new(0, true);
    let ov = Arc::new(OverlayProvider::new(base, ExactUpper::default()).unwrap());
    let f = VPath::at_default("a.txt");

    let first = {
        let ov = Arc::clone(&ov);
        std::thread::spawn(move || ov.open(f, OPEN_WRITE).map(|_| ()))
    };
    entered.recv_timeout(Duration::from_secs(10)).unwrap();

    let (tx, rx) = mpsc::channel();
    let second = {
        let ov = Arc::clone(&ov);
        std::thread::spawn(move || tx.send(ov.open(f, OPEN_WRITE).map(|(h, _, _)| h)).unwrap())
    };
    assert!(
        rx.recv_timeout(Duration::from_millis(150)).is_err(),
        "the second writer did not wait for the copy-up in flight"
    );
    gate.send(()).unwrap();
    assert_eq!(first.join().unwrap(), Err(map_io_err()));
    let h = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the waiter was never woken after the copy-up failed")
        .expect("the waiter's own copy-up should succeed");
    second.join().unwrap();
    ov.close(h).unwrap();
    assert_eq!(ov.getattr(f).unwrap().unwrap().size, 4);
}

/// The same relative path under two roots is two copy-ups: one parked in
/// the base for root 1 must not hold up root 2.
#[test]
fn copy_ups_of_the_same_path_under_two_roots_run_in_parallel() {
    use std::sync::mpsc;
    use std::time::Duration;
    use vfs_provider::RootId;
    let (base, entered, gate) = GatedBase::new(1, false);
    let ov = Arc::new(OverlayProvider::new(base, ExactUpper::default()).unwrap());

    let root1 = {
        let ov = Arc::clone(&ov);
        std::thread::spawn(move || {
            ov.open(VPath::new(RootId(1), "a.txt"), OPEN_WRITE)
                .map(|_| ())
        })
    };
    entered.recv_timeout(Duration::from_secs(10)).unwrap();

    let (tx, rx) = mpsc::channel();
    let root2 = {
        let ov = Arc::clone(&ov);
        std::thread::spawn(move || {
            tx.send(
                ov.open(VPath::new(RootId(2), "a.txt"), OPEN_WRITE)
                    .map(|_| ()),
            )
            .unwrap()
        })
    };
    let r2 = rx.recv_timeout(Duration::from_secs(10));
    gate.send(()).unwrap(); // release root 1 either way, so nothing hangs
    assert_eq!(
        r2.expect("root 2's copy-up waited on root 1's"),
        Ok(()),
        "root 2's copy-up failed"
    );
    root2.join().unwrap();
    assert_eq!(root1.join().unwrap(), Ok(()));
}

/// Gate 4, Task 6 review. The whiteout check runs on **every** read, and
/// the obvious implementation costs one `upper.getattr` per ancestor —
/// so a five-deep asset path pays six filesystem calls to answer a
/// question whose answer is "no" for the entire session. At a game load's
/// volume that is six figures of syscalls, in a harness whose other job
/// is measuring load time.
///
/// The budget asserted here is the whole point of the index: **one**
/// `upper.getattr` per warm `getattr` (the real content lookup, which was
/// always there), and **zero** `upper.readdir`. Correctness tests cannot
/// see this — the answers are the same either way.
#[test]
fn warm_reads_cost_one_upper_lookup_regardless_of_path_depth() {
    use vfs_provider::{Provider, VPath};
    let base = Arc::new(InlineProvider::from_files([
        ("a/b/c/d/deep.txt", b"DEEP".as_slice()),
        ("a/b/c/d/sibling.txt", b"SIB".as_slice()),
        ("shallow.txt", b"TOP".as_slice()),
    ]));
    let upper = Arc::new(CountingUpper::default());
    let ov = OverlayProvider::from_arcs(base, upper.clone()).unwrap();

    let deep = VPath::at_default("a/b/c/d/deep.txt");
    // Warm-up: this is where the per-directory scans happen, once.
    assert!(ov.getattr(deep).unwrap().is_some());
    let warm_getattrs = upper.getattrs.load(Ordering::Relaxed);
    let warm_readdirs = upper.readdirs.load(Ordering::Relaxed);
    assert!(
        warm_readdirs <= 5,
        "warm-up must scan at most one directory per path component \
         (5 for a 5-deep path), got {warm_readdirs}"
    );

    // Now the steady state: repeat reads, plus a sibling and an unrelated
    // shallow path, both of which reuse directories already scanned.
    for _ in 0..20 {
        assert!(ov.getattr(deep).unwrap().is_some());
    }
    assert!(ov
        .getattr(VPath::at_default("a/b/c/d/sibling.txt"))
        .unwrap()
        .is_some());
    assert!(ov
        .getattr(VPath::at_default("shallow.txt"))
        .unwrap()
        .is_some());
    // A path that does not exist anywhere must not reopen the question
    // either.
    assert!(ov
        .getattr(VPath::at_default("a/b/c/d/absent.txt"))
        .unwrap()
        .is_none());

    assert_eq!(
        upper.readdirs.load(Ordering::Relaxed),
        warm_readdirs,
        "a warm read must not touch the upper's directories at all; every call here \
         walks directories the index already holds"
    );
    assert_eq!(
        upper.getattrs.load(Ordering::Relaxed) - warm_getattrs,
        23,
        "each warm read must cost exactly one `upper.getattr` — the content lookup that \
         was always there — and none for the whiteout walk. A number near 6x this is the \
         per-ancestor `metadata` storm the index removes"
    );
}

/// The whiteout walk folds a path once and splits the fold at the same
/// separators as the path. That only works because a fold never adds or
/// removes a `/` — including for names whose fold changes length (`İ`,
/// two bytes, folds to three) or turns a non-ASCII character into an
/// ASCII one (the Kelvin sign folds to `k`). Markers on such names, at
/// every depth, must hide exactly what they hid when each ancestor was
/// folded on its own.
#[test]
fn whiteouts_hide_through_folds_that_change_a_names_length() {
    let base = Arc::new(InlineProvider::from_files([
        ("İstanbul/Sub/a.txt", b"1".as_slice()),
        ("İstanbul/Sub/keep.txt", b"2".as_slice()),
        ("İstanbul/other/b.txt", b"3".as_slice()),
        ("top/\u{212A}elvin/deep/c.txt", b"4".as_slice()),
        ("top/\u{212A}elvin/d.txt", b"5".as_slice()),
        ("top/plain/E.TXT", b"6".as_slice()),
        ("ÄÖ/ü.txt", b"7".as_slice()),
    ]));
    let upper = ExactUpper::default();
    // Markers spelled in a different case from the names they hide: one
    // on a file two levels down, one on a directory, one at the root.
    // (The directories are spelled as the base spells them: `ExactUpper` is
    // case-sensitive, and a directory's markers are read by the spelling of
    // the first lookup through it.)
    for marker in ["İstanbul/Sub/.wh.A.TXT", "top/.wh.kELVIN", ".wh.äö"] {
        let (h, _, _) = upper
            .open(VPath::at_default(marker), OPEN_WRITE | OPEN_CREATE)
            .unwrap();
        upper.close(h).unwrap();
    }
    let ov = OverlayProvider::new(base, upper).unwrap();
    let seen = |p: &str| ov.getattr(VPath::at_default(p)).unwrap().is_some();
    let opens = |p: &str| match ov.open(VPath::at_default(p), OPEN_READ) {
        Ok((h, _, _)) => {
            ov.close(h).unwrap();
            true
        }
        Err(e) => {
            assert_eq!(e, not_found());
            false
        }
    };
    // Twice: the first pass scans each directory, the second is answered
    // from the index.
    for _ in 0..2 {
        for (path, visible) in [
            ("İstanbul/Sub/a.txt", false),
            ("i\u{307}stanbul/sub/A.TXT", false),
            ("İstanbul/Sub/keep.txt", true),
            ("İstanbul/Sub", true),
            ("İstanbul/other/b.txt", true),
            ("top/\u{212A}elvin", false),
            ("top/\u{212A}elvin/d.txt", false),
            ("top/\u{212A}elvin/deep/c.txt", false),
            ("TOP/kelvin/DEEP/C.TXT", false),
            ("top/plain/E.TXT", true),
            ("top", true),
            ("ÄÖ", false),
            ("ÄÖ/ü.txt", false),
            ("äö/Ü.TXT", false),
        ] {
            assert_eq!(seen(path), visible, "getattr {path:?}");
            // (Files only: the test providers do not open directories.)
            if path.to_lowercase().ends_with(".txt") {
                assert_eq!(opens(path), visible, "open {path:?}");
            }
        }
    }
    // A directory under a whited-out one lists as gone; its sibling lists.
    assert_eq!(
        ov.readdir(VPath::at_default("top/\u{212A}elvin/deep")),
        Err(not_found())
    );
    let names: Vec<String> = ov
        .readdir(VPath::at_default("top"))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["plain"]);
}

/// The three questions the walk answers keep their separate meanings: a
/// marker on the path itself is not an ancestor's, and an ancestor's is
/// not the path's own.
#[test]
fn the_whiteout_walk_tells_a_paths_own_marker_from_an_ancestors() {
    let base = Arc::new(InlineProvider::from_files([
        ("a/b/c.txt", b"1".as_slice()),
        ("x/y/z.txt", b"2".as_slice()),
        ("top.txt", b"3".as_slice()),
    ]));
    let upper = ExactUpper::default();
    for marker in ["a/b/.wh.c.txt", ".wh.x", ".wh.top.txt"] {
        let (h, _, _) = upper
            .open(VPath::at_default(marker), OPEN_WRITE | OPEN_CREATE)
            .unwrap();
        upper.close(h).unwrap();
    }
    let ov = OverlayProvider::new(base, upper).unwrap();
    let ask = |p: &str| {
        let p = VPath::at_default(p);
        (
            ov.is_whiteout(p).unwrap(),
            ov.ancestor_whited_out(p).unwrap(),
            ov.hidden_by_whiteout(p).unwrap(),
        )
    };
    for _ in 0..2 {
        assert_eq!(ask("a/b/c.txt"), (true, false, true));
        assert_eq!(ask("a/b"), (false, false, false));
        assert_eq!(ask("a/b/other.txt"), (false, false, false));
        assert_eq!(ask("x"), (true, false, true));
        assert_eq!(ask("x/y"), (false, true, true));
        assert_eq!(ask("x/y/z.txt"), (false, true, true));
        assert_eq!(ask("top.txt"), (true, false, true));
        assert_eq!(ask("absent"), (false, false, false));
        assert_eq!(ask(""), (false, false, false));
    }
}

/// A directory's first scan runs outside the index lock. A whiteout
/// written in that window is in neither the scan (already read) nor the
/// index (the directory was not scanned yet, so there was no entry to
/// update); storing the scan afterwards would leave the removed file
/// visible to `getattr` and `open` for good. The remove is run from
/// inside the scan here, so the window is hit every time.
#[test]
fn a_whiteout_written_during_a_directorys_first_scan_still_hides() {
    use vfs_provider::{Provider, VPath};
    for clear in [false, true] {
        let base = Arc::new(InlineProvider::from_files([
            ("dir/a.txt", b"BASE".as_slice()),
            ("dir/b.txt", b"KEEP".as_slice()),
        ]));
        let upper = Arc::new(CountingUpper::default());
        let a = VPath::at_default("dir/a.txt");
        if clear {
            // The mirror image: the marker exists, and is cleared (by a
            // create over it) while the scan that saw it is in flight.
            let (h, _, _) = upper
                .open(VPath::at_default("dir/.wh.a.txt"), OPEN_WRITE | OPEN_CREATE)
                .unwrap();
            upper.close(h).unwrap();
        }
        let ov = Arc::new(OverlayProvider::from_arcs(base, upper.clone()).unwrap());
        let other = Arc::clone(&ov);
        *upper.after_readdir.lock().unwrap() = Some(Box::new(move || {
            if clear {
                let (h, _, _) = other.open(a, OPEN_WRITE | OPEN_CREATE).unwrap();
                other.close(h).unwrap();
            } else {
                other.remove(a).unwrap();
            }
        }));
        // This lookup scans `dir`; the change lands mid-scan.
        let _ = ov.getattr(a).unwrap();
        assert!(
            upper.after_readdir.lock().unwrap().is_none(),
            "the scan ran"
        );
        // Whatever that lookup answered, the change is complete now.
        assert_eq!(ov.getattr(a).unwrap().is_some(), clear, "clear={clear}");
        assert_eq!(
            ov.open(a, OPEN_READ).map(|(h, _, _)| ov.close(h).unwrap()),
            if clear { Ok(()) } else { Err(not_found()) },
            "clear={clear}"
        );
        assert!(ov
            .getattr(VPath::at_default("dir/b.txt"))
            .unwrap()
            .is_some());
        // Break the test's reference cycle (upper → hook → overlay).
        drop(ov);
    }
}

/// The index is only sound if it tracks the markers this provider writes.
/// A whiteout created *after* its directory was scanned must hide, and
/// clearing it must un-hide — otherwise the cache is a correctness bug
/// wearing a performance fix.
#[test]
fn a_whiteout_written_after_its_directory_was_scanned_still_hides() {
    use vfs_provider::{Provider, VPath, OPEN_CREATE, OPEN_WRITE};
    let base = Arc::new(InlineProvider::from_files([(
        "dir/a.txt",
        b"BASE".as_slice(),
    )]));
    let ov = OverlayProvider::new(base, ExactUpper::default()).unwrap();
    let f = VPath::at_default("dir/a.txt");

    // Read first, so "dir" and the root are already scanned and cached as
    // holding no markers.
    assert!(ov.getattr(f).unwrap().is_some());
    assert!(!ov.readdir(VPath::at_default("dir")).unwrap().is_empty());

    ov.remove(f).expect("remove writes a whiteout");
    assert!(
        ov.getattr(f).unwrap().is_none(),
        "a whiteout written after the directory was scanned did not hide the base file — \
         the index went stale"
    );

    // …and clearing it puts the path back.
    let (h, _, _) = ov.open(f, OPEN_WRITE | OPEN_CREATE).expect("recreate");
    ov.close(h).unwrap();
    assert!(
        ov.getattr(f).unwrap().is_some(),
        "clearing the whiteout did not remove it from the index"
    );
}

/// A caller can create a file literally named `.wh.x` through this
/// provider. The module docs reserve the prefix, so that file *is* a
/// marker for `x` — and `readdir`, which scans the upper live, treats it
/// as one. The index has to agree, or the two views of the same directory
/// disagree about whether `x` is hidden.
#[test]
fn creating_a_marker_named_file_through_the_overlay_is_seen_by_the_index() {
    use vfs_provider::{Provider, VPath, OPEN_CREATE, OPEN_WRITE};
    let base = Arc::new(InlineProvider::from_files([(
        "dir/x.txt",
        b"BASE".as_slice(),
    )]));
    let ov = OverlayProvider::new(base, ExactUpper::default()).unwrap();

    // Scan "dir" while it holds no markers.
    assert!(ov
        .getattr(VPath::at_default("dir/x.txt"))
        .unwrap()
        .is_some());

    let (h, _, _) = ov
        .open(VPath::at_default("dir/.wh.x.txt"), OPEN_WRITE | OPEN_CREATE)
        .expect("create a file whose name happens to be a marker");
    ov.close(h).unwrap();

    let listed = ov.readdir(VPath::at_default("dir")).unwrap();
    let listed_x = listed.iter().any(|e| e.name == "x.txt");
    let stat_x = ov
        .getattr(VPath::at_default("dir/x.txt"))
        .unwrap()
        .is_some();
    assert_eq!(
        listed_x, stat_x,
        "readdir and getattr disagree about whether `x.txt` is hidden: listed={listed_x}, \
         stat={stat_x}. readdir scans the upper live and the index must match it."
    );
}

#[test]
fn overlay_passes_write_conformance() {
    let base = Arc::new(InlineProvider::from_files(
        vfs_provider::FIXTURE_FILES.iter().copied(),
    ));
    let ov: Arc<dyn vfs_provider::Provider> =
        Arc::new(OverlayProvider::new(base, ExactUpper::default()).unwrap());
    vfs_provider::assert_conformance(ov);
}

#[test]
fn a_failed_copy_up_leaves_the_destination_absent_not_truncated() {
    use vfs_provider::{Provider, VPath, OPEN_CREATE, OPEN_WRITE};
    let base = Arc::new(FlakyReadBase {
        body: vec![7u8; 200_000],
    });
    let ov = OverlayProvider::new(base, ExactUpper::default()).unwrap();
    let f = VPath::at_default("big.bin");

    let err = ov
        .open(f, OPEN_WRITE)
        .expect_err("copy-up must fail when the base read dies partway through");
    assert_eq!(err, map_io_err());

    // The assertion that actually catches the bug: a truncated
    // destination is worse than an error, because every future
    // getattr/copy-up check would see "already present" and treat a
    // half-copied file as fully copied forever after. Checking only the
    // error status above would pass even with that bug intact.
    assert!(
        ov.upper.getattr(f).unwrap().is_none(),
        "a failed copy-up left a truncated file at the destination instead of nothing"
    );
    // And no orphaned `.cu.` temp file should linger either.
    assert!(
        ov.upper.readdir(VPath::at_default("")).unwrap().is_empty(),
        "a failed copy-up left a stray temp file behind in upper"
    );

    // Retrying after the transient failure is cleared works normally —
    // the failed attempt left the path genuinely untouched, not stuck.
    let ov2 = OverlayProvider::new(
        Arc::new(InlineProvider::from_files([("big.bin", b"ok".as_slice())])),
        ExactUpper::default(),
    )
    .unwrap();
    let (h, _, _) = ov2
        .open(f, OPEN_WRITE | OPEN_CREATE)
        .expect("unrelated retry works");
    ov2.close(h).unwrap();
}

#[test]
fn creating_under_a_removed_ancestor_directory_is_refused_until_mkdir_recreates_it() {
    use vfs_provider::{Provider, VPath, OPEN_CREATE, OPEN_WRITE};
    let base = Arc::new(InlineProvider::from_files([(
        "dir/a.txt",
        b"BASE".as_slice(),
    )]));
    let ov = OverlayProvider::new(base, ExactUpper::default()).unwrap();

    // Opaquely remove the whole base directory.
    ov.remove(VPath::at_default("dir"))
        .expect("whiteout the base directory");
    assert!(ov
        .getattr(VPath::at_default("dir/a.txt"))
        .unwrap()
        .is_none());

    // Creating a brand-new file underneath the removed directory is
    // refused outright, even with OPEN_CREATE. Clearing the ancestor's
    // whiteout here would silently resurrect every other base entry
    // under "dir" that nobody asked to restore; creating the file while
    // leaving the ancestor whiteout in place would leave it permanently
    // invisible to getattr while still surfacing through readdir's
    // upper merge. Refusing is the only option with no inconsistent
    // state; the way back is explicit.
    let err = ov
        .open(VPath::at_default("dir/new.txt"), OPEN_WRITE | OPEN_CREATE)
        .expect_err("create under a whited-out ancestor must be refused");
    assert_eq!(err, vfs_provider::not_found());

    // The explicit way back: mkdir clears exactly "dir"'s own whiteout.
    ov.mkdir(VPath::at_default("dir"))
        .expect("mkdir recreates the directory");
    let (h, _, _) = ov
        .open(VPath::at_default("dir/new.txt"), OPEN_WRITE | OPEN_CREATE)
        .expect("create succeeds once the ancestor is explicitly recreated");
    ov.close(h).unwrap();
    assert!(ov
        .getattr(VPath::at_default("dir/new.txt"))
        .unwrap()
        .is_some());
}
