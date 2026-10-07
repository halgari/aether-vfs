//! The shim's registry client (`vfs_shim::regclient`) against the director's own registry code.
//!
//! The far side of the ring is `fakedirector` with [`Fake::with_registry`]: the registry
//! opcodes are answered by `vfs_director`'s `dispatch_director` over a real `RegistryHost`, and
//! the ring header carries the director's registry generation as `IpcServe` publishes it. A
//! second `FuseClient` on the same section plays a second injected process.
//!
//! Every test takes [`LOCK`]: they share one director, and a write in one test moves the
//! generation every other test's cache is checked against, which would make "this read cost
//! no round trip" flaky rather than false.

use crate::fakedirector;
use crate::reg;

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use fakedirector::Fake;
use vfs_protocol::{
    OP_REG_KEY, OP_REG_LOOKUP, OP_REG_SET_VALUE, ST_BAD_REQUEST, ST_EXISTS, ST_IO_ERROR,
    ST_NOT_SUPPORTED, ST_REPLY_TOO_LARGE,
};
use vfs_registry::{Child, Lookup};
use vfs_shim::regclient::{self, RegClient};

static LOCK: Mutex<()> = Mutex::new(());

const REG_DWORD: u32 = 4;

struct Fixture {
    fake: &'static Fake,
    root: std::path::PathBuf,
}

fn fixture() -> (MutexGuard<'static, ()>, &'static Fixture) {
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    static F: OnceLock<Fixture> = OnceLock::new();
    let f = F.get_or_init(|| {
        // What the host sets while a registry layer is attached, read once by `enabled`.
        let (fake, root) = reg::start_director("regclient");
        // This binary drives the client without installing the hooks; record the outcome an
        // install with every registry detour in would, which `enabled` requires.
        regclient::detours_installed(&[]);
        Fixture { fake, root }
    });
    (guard, f)
}

fn key_path(test: &str) -> String {
    format!(r"\Registry\Machine\Software\RegClientTest\{test}")
}

#[test]
fn enabled_with_the_flag_and_a_director() {
    isolate!();
    let (_g, _f) = fixture();
    assert!(regclient::enabled());
    assert!(regclient::global().is_some());
}

/// Every call through the process's client reaches the director and comes back decoded.
#[test]
fn every_call_round_trips() {
    isolate!();
    let (_g, _f) = fixture();
    let k = key_path("RoundTrip");

    assert_eq!(regclient::lookup(&k), Ok((Lookup::Absent, false)));
    assert_eq!(regclient::key(&k), Ok(None));

    regclient::set_value(&k, "Width", REG_DWORD, &1920u32.to_le_bytes()).unwrap();
    let node = regclient::key(&k).unwrap().expect("node after a set");
    assert_eq!(node.values.len(), 1);
    assert_eq!(node.values[0].name, "Width");
    assert_eq!(node.values[0].ty, REG_DWORD);
    assert_eq!(node.values[0].data, 1920u32.to_le_bytes());
    assert_eq!(
        regclient::lookup(&k),
        Ok((Lookup::Present { created: false }, false))
    );
    // The parent sees something below it.
    let parent = key_path("");
    let parent = parent.trim_end_matches('\\');
    assert!(regclient::lookup(parent).unwrap().1);

    regclient::delete_value(&k, "WIDTH").unwrap();
    let node = regclient::key(&k).unwrap().unwrap();
    assert!(node.values.is_empty());
    assert_eq!(node.value_tombstones, vec!["width".to_string()]);

    let sub = format!(r"{k}\Sub");
    regclient::create_key(&sub, false).unwrap();
    assert_eq!(
        regclient::lookup(&sub),
        Ok((Lookup::Present { created: true }, false))
    );
    assert_eq!(regclient::create_key(&sub, false), Err(ST_EXISTS));

    regclient::rename_key(&sub, "Renamed").unwrap();
    assert_eq!(regclient::lookup(&sub), Ok((Lookup::Tombstoned, false)));
    let renamed = format!(r"{k}\Renamed");
    assert_eq!(
        regclient::lookup(&renamed),
        Ok((Lookup::Present { created: true }, false))
    );
    let children = regclient::key(&k).unwrap().unwrap().children;
    assert_eq!(
        children.get("renamed"),
        Some(&("Renamed".to_string(), Child::Present))
    );

    let (_, before) = regclient::changed(&k, true, 0).unwrap();
    regclient::delete_key(&renamed).unwrap();
    assert_eq!(regclient::lookup(&renamed), Ok((Lookup::Tombstoned, false)));
    let (changed, after) = regclient::changed(&k, true, before).unwrap();
    assert!(changed, "a delete below the key is a change under it");
    assert!(after > before);
    assert_eq!(regclient::changed(&k, true, after), Ok((false, after)));
}

/// A cached answer costs no round trip, and a write by another process (another client on the
/// same ring) makes it unusable for every process before that write returns.
#[test]
fn a_write_by_another_process_invalidates_every_cache() {
    isolate!();
    let (_g, f) = fixture();
    let k = key_path("CrossProcess");
    let a = regclient::global().unwrap();
    let other = fakedirector::second_client(&f.root);
    let b = RegClient::new(&other);
    let reqs = |op| f.fake.tally.reg(op, &k);

    // A miss, then hits.
    assert_eq!(a.lookup(&k), Ok((Lookup::Absent, false)));
    assert_eq!(a.lookup(&k), Ok((Lookup::Absent, false)));
    assert_eq!(a.key(&k), Ok(None));
    assert_eq!(a.key(&k), Ok(None));
    assert_eq!(reqs(OP_REG_LOOKUP), 1, "the second lookup was a cache hit");
    assert_eq!(reqs(OP_REG_KEY), 1, "the second key read was a cache hit");
    // The other process caches its own answers.
    assert_eq!(b.key(&k), Ok(None));
    assert_eq!(b.key(&k), Ok(None));
    assert_eq!(reqs(OP_REG_KEY), 2);

    // B writes; A's next read asks again and sees it.
    b.set_value(&k, "Fov", REG_DWORD, &90u32.to_le_bytes())
        .unwrap();
    let node = a.key(&k).unwrap().expect("A sees B's write");
    assert_eq!(node.values[0].data, 90u32.to_le_bytes());
    assert_eq!(
        a.lookup(&k),
        Ok((Lookup::Present { created: false }, false))
    );
    assert_eq!(reqs(OP_REG_KEY), 3);
    assert_eq!(reqs(OP_REG_LOOKUP), 2);
    // ... and caches the new answer.
    a.key(&k).unwrap();
    assert_eq!(reqs(OP_REG_KEY), 3);

    // And the other way round: A writes, B's cached answer is not used.
    assert_eq!(b.key(&k).unwrap().unwrap().values.len(), 1);
    a.delete_value(&k, "Fov").unwrap();
    assert!(b.key(&k).unwrap().unwrap().values.is_empty());
    // A writer's own cache is invalidated the same way.
    assert!(a.key(&k).unwrap().unwrap().values.is_empty());
}

/// A director with no registry layer answers `ST_NOT_SUPPORTED`: reads and writes fail, and an
/// answer cached while the layer was attached is not served after it is detached.
#[test]
fn no_registry_layer_fails_reads_and_writes() {
    isolate!();
    let (_g, f) = fixture();
    let k = key_path("Detached");
    regclient::set_value(&k, "v", REG_DWORD, &1u32.to_le_bytes()).unwrap();
    assert!(regclient::key(&k).unwrap().is_some());
    regclient::lookup(&k).unwrap();

    let host = f.fake.director().registry().unwrap();
    f.fake.director().set_registry(None);
    let fallbacks = vfs_shim::reg_read_fallback_count();
    let result = (
        regclient::lookup(&k),
        regclient::key(&k).map(|n| n.is_some()),
        regclient::set_value(&k, "v", REG_DWORD, &2u32.to_le_bytes()),
        regclient::delete_value(&k, "v"),
        regclient::create_key(&k, false),
        regclient::delete_key(&k),
        regclient::rename_key(&k, "Other"),
        regclient::changed(&k, false, 0),
    );
    let fallbacks = vfs_shim::reg_read_fallback_count() - fallbacks;
    f.fake.director().set_registry(Some(host));

    assert_eq!(result.0, Err(ST_NOT_SUPPORTED), "lookup");
    assert_eq!(result.1, Err(ST_NOT_SUPPORTED), "key");
    assert_eq!(result.2, Err(ST_NOT_SUPPORTED), "set_value");
    assert_eq!(result.3, Err(ST_NOT_SUPPORTED), "delete_value");
    assert_eq!(result.4, Err(ST_NOT_SUPPORTED), "create_key");
    assert_eq!(result.5, Err(ST_NOT_SUPPORTED), "delete_key");
    assert_eq!(result.6, Err(ST_NOT_SUPPORTED), "rename_key");
    assert_eq!(result.7, Err(ST_NOT_SUPPORTED), "changed");
    assert_eq!(fallbacks, 2, "both failed reads counted as fallbacks");

    // Reattached, the same layer answers again.
    assert!(regclient::key(&k).unwrap().is_some());
}

/// A key whose `REG_KEY` answer does not fit the ring is a read failure the caller falls back
/// on, not an error that breaks the key.
#[test]
fn a_key_too_large_for_the_ring_is_a_read_failure() {
    isolate!();
    let (_g, _f) = fixture();
    let k = key_path("TooLarge");
    // Three values that each fit a request but together overflow a 4 KiB reply.
    for name in ["a", "b", "c"] {
        regclient::set_value(&k, name, 3, &[0x5a; 1500]).unwrap();
    }
    let fallbacks = vfs_shim::reg_read_fallback_count();
    assert_eq!(regclient::key(&k), Err(ST_REPLY_TOO_LARGE));
    assert_eq!(vfs_shim::reg_read_fallback_count() - fallbacks, 1);
    // The lookup is small and still answers.
    assert_eq!(
        regclient::lookup(&k),
        Ok((Lookup::Present { created: false }, false))
    );
}

/// A director that never answers: every call fails (after the client's deadline), reads are
/// counted as fallbacks, and nothing hangs.
#[test]
fn a_dead_director_fails_reads_and_writes() {
    isolate!();
    let (_g, f) = fixture();
    let dead = fakedirector::unserved_client(&f.root, Duration::from_millis(200));
    let c = RegClient::new(&dead);
    let k = key_path("Dead");
    let fallbacks = vfs_shim::reg_read_fallback_count();
    assert_eq!(c.lookup(&k), Err(ST_IO_ERROR));
    assert_eq!(c.key(&k), Err(ST_IO_ERROR));
    assert_eq!(vfs_shim::reg_read_fallback_count() - fallbacks, 2);
    assert_eq!(c.set_value(&k, "v", REG_DWORD, &[0; 4]), Err(ST_IO_ERROR));
    assert_eq!(c.delete_value(&k, "v"), Err(ST_IO_ERROR));
    assert_eq!(c.create_key(&k, false), Err(ST_IO_ERROR));
    assert_eq!(c.delete_key(&k), Err(ST_IO_ERROR));
    assert_eq!(c.rename_key(&k, "Other"), Err(ST_IO_ERROR));
    assert_eq!(c.changed(&k, false, 0), Err(ST_IO_ERROR));
}

/// A write whose request cannot fit one ring payload is refused by the client with
/// `ST_BAD_REQUEST` and never sent; one that exactly fits is sent and applied.
#[test]
fn a_request_too_large_for_the_ring_is_refused_unsent() {
    isolate!();
    let (_g, f) = fixture();
    let k = key_path("Oversize");
    // `path | name | ty | len | data`, each string with a 4-byte length.
    let overhead = 4 + k.len() + 4 + 1 + 4 + 4;
    let fits = vec![0x11u8; fakedirector::PAYLOAD_CAP as usize - overhead];
    let over = vec![0x22u8; fits.len() + 1];

    assert_eq!(regclient::set_value(&k, "v", 3, &over), Err(ST_BAD_REQUEST));
    assert_eq!(f.fake.tally.reg(OP_REG_SET_VALUE, &k), 0, "never submitted");

    regclient::set_value(&k, "v", 3, &fits).unwrap();
    assert_eq!(f.fake.tally.reg(OP_REG_SET_VALUE, &k), 1);
}
