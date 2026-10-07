//! The registry query hooks: `NtQueryKey`, `NtEnumerateKey`, `NtQueryValueKey`,
//! `NtEnumerateValueKey` and `NtQueryMultipleValueKey` on keys the overlay touches (registry
//! overlay spec sections 3.2, 3.3, 3.5 and 6), through the real ntdll entry points with the
//! shim's detours installed and a real director registry behind the ring.
//!
//! Two oracles:
//! - **The host itself.** `M\Merge` is a real key with overlay changes; `X\Merge` is a real
//!   key, never touched, built before the hooks with the content the merge should show. Where
//!   the host's layout and Windows' agree, the merged answer must be the host's answer for the
//!   equivalent key, byte for byte (last-write times masked: the two keys were written at
//!   different moments).
//! - **`vfs_registry::layout`**, for partial buffers and the classes where the host's layout
//!   and the Windows x64 layout differ: the expected answer is the layout writer's on the
//!   expected merged view.
//!
//! Every test takes [`LOCK`]: they share one director, and one of them detaches its registry.

use crate::fakedirector;
use crate::reg;

use std::ops::Deref;
use std::sync::{Mutex, MutexGuard, OnceLock};

use fakedirector::Fake;
use reg::{Paths, UnicodeString, open_abs, reg_create_class, wide};
use vfs_registry::layout::{self, KeyInfoClass, ValueEntry, ValueInfoClass, Written};
use vfs_registry::{MergedKey, Value};
use vfs_shim::{is_synthetic_key_handle, regclient, registry_enum_states};
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegOpenKeyExW, RegQueryInfoKeyW,
    RegQueryValueExW, RegSetValueExW,
};

static LOCK: Mutex<()> = Mutex::new(());

const BASE: &str = r"Software\AetherVfsRegQueryTest";

const STATUS_SUCCESS: i32 = 0;
const STATUS_NO_MORE_ENTRIES: i32 = 0x8000_001Au32 as i32;
const STATUS_INVALID_PARAMETER: i32 = 0xC000_000Du32 as i32;
const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
const STATUS_BUFFER_TOO_SMALL: i32 = 0xC000_0023u32 as i32;
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
const STATUS_KEY_DELETED: i32 = 0xC000_017Cu32 as i32;

const KEY_QUERY_VALUE: u32 = 0x1;
const KEY_ENUMERATE_SUB_KEYS: u32 = 0x8;
const NT_KEY_READ: u32 = 0x2_0019;

const REG_SZ: u32 = 1;
const REG_BINARY: u32 = 3;
const REG_DWORD: u32 = 4;

/// Sentinel for bytes a call must not touch.
const S: u8 = 0xCC;

/// x64 `KEY_VALUE_ENTRY`.
#[repr(C)]
#[derive(Clone, Copy)]
struct KeyValueEntry {
    value_name: *const UnicodeString,
    data_length: u32,
    data_offset: u32,
    ty: u32,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtClose(h: isize) -> i32;
    fn NtQueryKey(h: isize, class: u32, info: *mut u8, len: u32, ret: *mut u32) -> i32;
    fn NtEnumerateKey(
        h: isize,
        index: u32,
        class: u32,
        info: *mut u8,
        len: u32,
        ret: *mut u32,
    ) -> i32;
    fn NtQueryValueKey(
        h: isize,
        name: *const UnicodeString,
        class: u32,
        info: *mut u8,
        len: u32,
        ret: *mut u32,
    ) -> i32;
    fn NtEnumerateValueKey(
        h: isize,
        index: u32,
        class: u32,
        info: *mut u8,
        len: u32,
        ret: *mut u32,
    ) -> i32;
    fn NtQueryMultipleValueKey(
        h: isize,
        entries: *mut KeyValueEntry,
        count: u32,
        buffer: *mut u8,
        buffer_len: *mut u32,
        required: *mut u32,
    ) -> i32;
}

/// `s` as REG_SZ data: UTF-16 with its terminating NUL.
fn sz(s: &str) -> Vec<u8> {
    wide(s).iter().flat_map(|u| u.to_le_bytes()).collect()
}

fn dword(v: u32) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

fn val(name: &str, ty: u32, data: &[u8]) -> Value {
    Value {
        name: name.into(),
        ty,
        data: data.into(),
    }
}

fn us(units: &[u16]) -> UnicodeString {
    UnicodeString {
        length: (units.len() * 2) as u16,
        maximum_length: (units.len() * 2) as u16,
        buffer: units.as_ptr(),
    }
}

// ---- Real keys, made before the hooks ----

/// Create `BASE\rel` (with `class`) and set `values` on it, in order.
fn make(rel: &str, class: Option<&str>, values: &[Value]) {
    let k = reg_create_class(&format!(r"{BASE}\{rel}"), class);
    for v in values {
        let st = unsafe {
            RegSetValueExW(
                k,
                wide(&v.name).as_ptr(),
                0,
                v.ty,
                v.data.as_ptr(),
                v.data.len() as u32,
            )
        };
        assert_eq!(st, 0, "RegSetValueExW {rel} {}", v.name);
    }
    unsafe { RegCloseKey(k) };
}

fn last_write_of(rel: &str) -> u64 {
    // Opened, not created: a create of an existing key with no class clears its class on Wine.
    let mut k: HKEY = std::ptr::null_mut();
    let st = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide(&format!(r"{BASE}\{rel}")).as_ptr(),
            0,
            KEY_READ,
            &mut k,
        )
    };
    assert_eq!(st, 0);
    let mut ft = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let n = std::ptr::null_mut();
    let st =
        unsafe { RegQueryInfoKeyW(k, std::ptr::null_mut(), n, n, n, n, n, n, n, n, n, &mut ft) };
    assert_eq!(st, 0);
    unsafe { RegCloseKey(k) };
    (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64
}

/// The real values of `M\Merge`, in the order they are written.
fn merge_real_values() -> Vec<Value> {
    vec![
        val("B", REG_SZ, &sz("real-b")),
        val("C", REG_DWORD, &dword(3)),
        val("D", REG_BINARY, &[7; 100]),
        val("E", REG_SZ, &sz("e")),
    ]
}

/// The values `M\Merge` shows once merged (and `X\Merge` really has), in merge order.
fn merged_values() -> Vec<Value> {
    vec![
        val("A_new", REG_SZ, &sz("overlay-a")),
        val("b", REG_DWORD, &dword(7)),
        val("C", REG_DWORD, &dword(3)),
        val("E", REG_SZ, &sz("e")),
    ]
}

fn build_real_keys() {
    reg::reset_base(BASE, &["Limited"]);
    // The key the overlay changes, and its children.
    make(r"M\Merge", Some("MCls"), &merge_real_values());
    make(r"M\Merge\Sa", None, &[val("x", REG_DWORD, &dword(1))]);
    make(
        r"M\Merge\Sb",
        Some("SbCls"),
        &[val("y", REG_DWORD, &dword(2))],
    );
    make(r"M\Merge\Sb\Deep", None, &[]);
    make(r"M\Merge\SgoneLongName", None, &[]);
    // The same key as the merge should show it, never touched by the overlay.
    make(r"X\Merge", Some("MCls"), &merged_values());
    make(r"X\Merge\Sa", None, &[val("x", REG_DWORD, &dword(1))]);
    make(
        r"X\Merge\Sb",
        Some("SbCls"),
        &[
            val("y", REG_DWORD, &dword(2)),
            val("z", REG_DWORD, &dword(9)),
        ],
    );
    make(r"X\Merge\Sb\Deep", None, &[]);
    make(r"X\Merge\Sc", None, &[]);
    // Deleted, then created again in the overlay.
    make("Revived", Some("RCls"), &[val("v", REG_SZ, &sz("old"))]);
    make(r"Revived\Old", None, &[]);
    // Untouched: a pass-through handle.
    make("Plain", None, &[val("p", REG_DWORD, &dword(5))]);
    make(r"Plain\PSub", None, &[]);
    // Untouched until a test writes to the overlay through regclient (Task 11's copy-on-write).
    make("Cow", None, &[val("c", REG_DWORD, &dword(1))]);
    make("CowQ", None, &[val("c", REG_DWORD, &dword(1))]);
    make(r"CowQ\K", None, &[]);
    make("Doomed", None, &[]);
    make("Access", None, &[val("r", REG_DWORD, &dword(1))]);
    make(r"Access\Sub", None, &[]);
    make("Live", None, &[]);
    make(r"Live\A", None, &[]);
    make(r"Live\B", None, &[]);
    make("Down", None, &[val("r", REG_DWORD, &dword(1))]);
    // Its overlay node is too large for a REG_KEY reply: the lookup answers, the node read fails.
    make("TooBig", None, &[val("r", REG_DWORD, &dword(1))]);
    make(r"TooBig\R1", None, &[]);
    make(r"TooBig\R2", None, &[]);
    // Opened before the hooks (an untracked handle).
    make("Pre", None, &[val("p", REG_DWORD, &dword(1))]);
    make(r"Pre\PreSub", None, &[]);
    // Grants KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS only: KEY_READ is refused.
    make("Limited", None, &[val("l", REG_DWORD, &dword(1))]);
    make(r"Limited\LSub", None, &[]);
    reg::set_dacl(&format!(r"{BASE}\Limited"), "D:P(A;;0x9;;;WD)");
}

/// Whether a Win32 open of `BASE\rel` with `access` succeeds (before the hooks).
fn grants(rel: &str, access: u32) -> bool {
    let mut k: HKEY = std::ptr::null_mut();
    let st = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide(&format!(r"{BASE}\{rel}")).as_ptr(),
            0,
            access,
            &mut k,
        )
    };
    if st == 0 {
        unsafe { RegCloseKey(k) };
    }
    st == 0
}

struct Fixture {
    fake: &'static Fake,
    paths: Paths,
    /// `M\Merge`'s real last-write time, read before the hooks.
    merge_real_lw: u64,
    /// `Pre`, opened `KEY_ENUMERATE_SUB_KEYS` before the hooks: neither table knows it.
    pre: isize,
    /// `Limited` refused KEY_READ and granted KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS before
    /// the hooks.
    limited_as_expected: bool,
}

impl Deref for Fixture {
    type Target = Paths;
    fn deref(&self) -> &Paths {
        &self.paths
    }
}

impl Fixture {
    fn open(&self, rel: &str, access: u32) -> isize {
        let (st, h) = open_abs(&self.nt(rel), access);
        assert_eq!(st, STATUS_SUCCESS, "open {rel}");
        h
    }
}

fn fixture() -> (MutexGuard<'static, ()>, &'static Fixture) {
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    static F: OnceLock<Fixture> = OnceLock::new();
    let f = F.get_or_init(|| {
        build_real_keys();
        let merge_real_lw = last_write_of(r"M\Merge");
        let limited_as_expected = !grants("Limited", KEY_READ)
            && grants("Limited", KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS);
        let paths = Paths::new(BASE);
        let (st, pre) = open_abs(&paths.nt("Pre"), KEY_ENUMERATE_SUB_KEYS);
        assert_eq!(st, STATUS_SUCCESS);
        let fake = reg::install_hooks("regquery");
        let f = Fixture {
            fake,
            paths,
            merge_real_lw,
            pre,
            limited_as_expected,
        };
        // The overlay's changes to `M\Merge`: a new value, a value shadowed in another case,
        // a tombstoned value, a tombstoned subkey, a subkey created here, and a value below a
        // real subkey (which makes it touched).
        let m = f.canon(r"M\Merge");
        regclient::set_value(&m, "A_new", REG_SZ, &sz("overlay-a")).unwrap();
        regclient::set_value(&m, "b", REG_DWORD, &dword(7)).unwrap();
        regclient::delete_value(&m, "d").unwrap();
        regclient::delete_key(&format!(r"{m}\sgonelongname")).unwrap();
        regclient::create_key(&format!(r"{m}\Sc"), false).unwrap();
        regclient::set_value(&format!(r"{m}\Sb"), "z", REG_DWORD, &dword(9)).unwrap();
        // `Revived`: deleted and created again, with content of its own.
        let r = f.canon("Revived");
        regclient::delete_key(&r).unwrap();
        regclient::create_key(&r, false).unwrap();
        regclient::set_value(&r, "n", REG_DWORD, &dword(1)).unwrap();
        regclient::create_key(&format!(r"{r}\New"), false).unwrap();
        for k in ["Doomed", "Access", "Live", "Down", "Limited"] {
            regclient::set_value(&f.canon(k), "o", REG_DWORD, &dword(2)).unwrap();
        }
        for name in ["a", "b", "c"] {
            regclient::set_value(&f.canon("TooBig"), name, REG_BINARY, &[0x5a; 1500]).unwrap();
        }
        regclient::create_key(&f.canon(r"Pre\Added"), false).unwrap();
        f
    });
    (guard, f)
}

// ---- Calling, with sentinels ----

/// One call's answer: status, `ResultLength` and the bytes of the caller's buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ans {
    st: i32,
    rl: u32,
    bytes: Vec<u8>,
}

/// Untouched `ResultLength`.
const NO_RL: u32 = 0xDEAD_BEEF;

/// Run `f` on an 8-aligned, sentinel-filled buffer of `len` bytes (16 more behind it, which
/// must stay untouched).
fn call(len: usize, f: impl FnOnce(*mut u8, u32, *mut u32) -> i32) -> Ans {
    let words = (len + 16).div_ceil(8);
    let mut v = vec![u64::from_ne_bytes([S; 8]); words];
    let mut rl = NO_RL;
    let st = f(v.as_mut_ptr().cast(), len as u32, &mut rl);
    let bytes = unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, words * 8) };
    assert!(
        bytes[len..].iter().all(|&b| b == S),
        "wrote past the buffer"
    );
    Ans {
        st,
        rl,
        bytes: bytes[..len].to_vec(),
    }
}

/// The layout's answer for a buffer of `len` bytes.
fn lay(len: usize, f: impl FnOnce(&mut [u8]) -> Written) -> Ans {
    let mut v = vec![S; len];
    let w = f(&mut v);
    Ans {
        st: w.status,
        rl: w.result_length,
        bytes: v,
    }
}

/// Zero the LastWriteTime of a key class answer (whatever of it the buffer holds: Wine writes
/// part of the fixed structure even when it answers `STATUS_BUFFER_TOO_SMALL`).
fn mask_lw(mut a: Ans) -> Ans {
    let n = a.bytes.len().min(8);
    a.bytes[..n].fill(0);
    a
}

fn qkey(h: isize, class: u32, len: usize) -> Ans {
    call(len, |b, l, r| unsafe { NtQueryKey(h, class, b, l, r) })
}

fn ekey(h: isize, index: u32, class: u32, len: usize) -> Ans {
    call(len, |b, l, r| unsafe {
        NtEnumerateKey(h, index, class, b, l, r)
    })
}

fn qval(h: isize, name: &str, class: u32, len: usize) -> Ans {
    let w: Vec<u16> = name.encode_utf16().collect();
    let u = us(&w);
    call(len, |b, l, r| unsafe {
        NtQueryValueKey(h, &u, class, b, l, r)
    })
}

fn eval(h: isize, index: u32, class: u32, len: usize) -> Ans {
    call(len, |b, l, r| unsafe {
        NtEnumerateValueKey(h, index, class, b, l, r)
    })
}

/// The lengths worth trying for an answer of `total` bytes: none, around each fixed part,
/// one short, exact, and roomy.
fn lens(total: u32) -> Vec<usize> {
    let t = total as usize;
    let mut v = vec![
        0, 1, 3, 4, 7, 8, 11, 12, 15, 16, 19, 20, 23, 24, 27, 28, 39, 40, 43, 44,
    ];
    v.extend([t.saturating_sub(3), t.saturating_sub(1), t, t + 1, t + 8]);
    v.sort();
    v.dedup();
    v
}

/// `KEY_BASIC_INFORMATION` names from enumerating `h` with Basic, in order.
fn subkey_names(h: isize) -> Vec<String> {
    let mut out = vec![];
    for i in 0.. {
        let a = ekey(h, i, 0, 512);
        if a.st == STATUS_NO_MORE_ENTRIES {
            break;
        }
        assert_eq!(a.st, STATUS_SUCCESS, "enumerate {i}");
        let n = u32::from_le_bytes(a.bytes[12..16].try_into().unwrap()) as usize;
        out.push(utf16(&a.bytes[16..16 + n]));
    }
    out
}

fn value_names(h: isize) -> Vec<String> {
    let mut out = vec![];
    for i in 0.. {
        let a = eval(h, i, 0, 512);
        if a.st == STATUS_NO_MORE_ENTRIES {
            break;
        }
        assert_eq!(a.st, STATUS_SUCCESS, "enumerate value {i}");
        let n = u32::from_le_bytes(a.bytes[8..12].try_into().unwrap()) as usize;
        out.push(utf16(&a.bytes[12..12 + n]));
    }
    out
}

fn utf16(b: &[u8]) -> String {
    let u: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&u)
}

fn class_units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// `M\Merge` as the merge should show it.
fn merge_expected(last_write: u64) -> MergedKey {
    MergedKey {
        subkeys: vec!["Sa".into(), "Sb".into(), "Sc".into()],
        values: merged_values(),
        class: Some(class_units("MCls")),
        last_write,
        // "SbCls", the longest real subkey class.
        max_subkey_class_len: 10,
    }
}

fn lw_of(a: &Ans) -> u64 {
    u64::from_le_bytes(a.bytes[..8].try_into().unwrap())
}

// ---- Tests ----

#[test]
fn query_key_answers_like_the_equivalent_real_key() {
    isolate!();
    let (_g, f) = fixture();
    let m = f.open(r"M\Merge", NT_KEY_READ);
    let x = f.open(r"X\Merge", NT_KEY_READ);
    assert!(is_synthetic_key_handle(m));
    assert!(!is_synthetic_key_handle(x), "the mirror is untouched");
    // Node is left out: Wine puts the class right after the name, Windows (and the layout)
    // at the next 4-byte boundary. `query_key_partial_buffers_follow_the_layout` covers it.
    for class in [0u32, 2, 4] {
        let got = qkey(m, class, 512);
        let mut want = qkey(x, class, 512);
        assert_eq!(got.st, STATUS_SUCCESS, "class {class}");
        if class == 4 {
            // Bytes 36..40 of KEY_CACHED_INFORMATION are padding on Windows, left alone by
            // the layout; Wine writes a value there.
            want.bytes[36..40].copy_from_slice(&got.bytes[36..40]);
        }
        assert_eq!(
            mask_lw(got.clone()),
            mask_lw(want),
            "class {class}: the host's answer for the equivalent key"
        );
        // The later of the real key's and the overlay node's last-write times.
        let node_lw = regclient::key(&f.canon(r"M\Merge"))
            .unwrap()
            .unwrap()
            .last_write;
        assert_eq!(lw_of(&got), f.merge_real_lw.max(node_lw), "class {class}");
    }
    NtCloseAll(&[m, x]);
}

#[allow(non_snake_case)]
fn NtCloseAll(hs: &[isize]) {
    for &h in hs {
        assert_eq!(unsafe { NtClose(h) }, STATUS_SUCCESS);
    }
}

#[test]
fn query_key_partial_buffers_follow_the_layout() {
    isolate!();
    let (_g, f) = fixture();
    let m = f.open(r"M\Merge", NT_KEY_READ);
    let lw = lw_of(&qkey(m, 0, 512));
    let k = merge_expected(lw);
    for kc in [
        KeyInfoClass::Basic,
        KeyInfoClass::Node,
        KeyInfoClass::Full,
        KeyInfoClass::Cached,
    ] {
        let total = lay(512, |b| layout::write_key_info(kc, &k, "Merge", b)).rl;
        for len in lens(total) {
            let want = lay(len, |b| layout::write_key_info(kc, &k, "Merge", b));
            assert_eq!(qkey(m, kc as u32, len), want, "{kc:?} len {len}");
        }
    }
    // The name is the NT path.
    let name = f.nt(r"M\Merge");
    let total = lay(1024, |b| {
        layout::write_key_info(KeyInfoClass::Name, &k, &name, b)
    })
    .rl;
    for len in lens(total) {
        let want = lay(len, |b| {
            layout::write_key_info(KeyInfoClass::Name, &k, &name, b)
        });
        assert_eq!(qkey(m, 3, len), want, "Name len {len}");
    }
    NtCloseAll(&[m]);
}

#[test]
fn classes_the_layout_does_not_own_go_to_the_real_key() {
    isolate!();
    let (_g, f) = fixture();
    let m = f.open(r"M\Merge", NT_KEY_READ);
    let x = f.open(r"X\Merge", NT_KEY_READ);
    // Flags, Virtualization, HandleTags, Trust, Layer, and a class past every known one: the
    // host's own answer for a real key.
    for class in [5u32, 6, 7, 8, 9, 100] {
        for len in [0usize, 4, 64] {
            let got = qkey(m, class, len);
            let want = qkey(x, class, len);
            assert_eq!(got.st, want.st, "class {class} len {len}");
            assert_eq!(got.bytes, want.bytes, "class {class} len {len}");
        }
    }
    // A key created here has no real key: the layout's zeroed answers, else invalid.
    let c = f.open(r"M\Merge\Sc", NT_KEY_READ);
    assert!(is_synthetic_key_handle(c));
    for (class, size) in [(5u32, 12usize), (6, 4), (7, 4)] {
        assert_eq!(
            qkey(c, class, size + 4),
            Ans {
                st: STATUS_SUCCESS,
                rl: size as u32,
                bytes: [vec![0; size], vec![S; 4]].concat()
            },
            "class {class}"
        );
        assert_eq!(qkey(c, class, size - 1).st, STATUS_BUFFER_TOO_SMALL);
    }
    for class in [8u32, 9, 100] {
        assert_eq!(
            qkey(c, class, 64).st,
            STATUS_INVALID_PARAMETER,
            "class {class}"
        );
    }
    NtCloseAll(&[m, x, c]);
}

#[test]
fn enumerate_key_lists_real_then_created_subkeys_minus_tombstones() {
    isolate!();
    let (_g, f) = fixture();
    let m = f.open(r"M\Merge", NT_KEY_READ);
    let x = f.open(r"X\Merge", NT_KEY_READ);
    assert_eq!(subkey_names(m), ["Sa", "Sb", "Sc"]);
    assert_eq!(ekey(m, 3, 0, 64).st, STATUS_NO_MORE_ENTRIES);
    // Each entry, each class: an untouched real subkey (Sa), a touched one (Sb: its own merged
    // counts) and one created here (Sc).
    for i in 0..3u32 {
        for class in 0..3u32 {
            if class == 1 && i == 1 {
                // Sb's Node answer is the layout's: its class is placed as on Windows, where
                // Wine places it unaligned (below).
                continue;
            }
            let got = ekey(m, i, class, 512);
            assert_eq!(got.st, STATUS_SUCCESS, "index {i} class {class}");
            assert_eq!(
                mask_lw(got),
                mask_lw(ekey(x, i, class, 512)),
                "index {i} class {class}"
            );
        }
    }
    // Partial buffers of the entries the merge writes, against the layout.
    let sb = f.open(r"M\Merge\Sb", NT_KEY_READ);
    let sb_lw = lw_of(&qkey(sb, 0, 64));
    let sb_view = MergedKey {
        subkeys: vec!["Deep".into()],
        values: vec![
            val("z", REG_DWORD, &dword(9)),
            val("y", REG_DWORD, &dword(2)),
        ],
        class: Some(class_units("SbCls")),
        last_write: sb_lw,
        max_subkey_class_len: 0,
    };
    let sc = f.open(r"M\Merge\Sc", NT_KEY_READ);
    let sc_view = MergedKey {
        last_write: lw_of(&qkey(sc, 0, 64)),
        ..MergedKey::default()
    };
    for (i, name, view) in [(1u32, "Sb", &sb_view), (2, "Sc", &sc_view)] {
        for kc in [KeyInfoClass::Basic, KeyInfoClass::Node, KeyInfoClass::Full] {
            let total = lay(512, |b| layout::write_subkey_info(kc, name, view, b)).rl;
            for len in lens(total) {
                let want = lay(len, |b| layout::write_subkey_info(kc, name, view, b));
                assert_eq!(ekey(m, i, kc as u32, len), want, "{name} {kc:?} len {len}");
            }
        }
    }
    // Partial buffers of a forwarded entry are the host's.
    for class in 0..3u32 {
        let total = ekey(x, 0, class, 512).rl;
        for len in lens(total) {
            assert_eq!(
                mask_lw(ekey(m, 0, class, len)),
                mask_lw(ekey(x, 0, class, len)),
                "Sa class {class} len {len}"
            );
        }
    }
    // Only Basic, Node and Full enumerate.
    for class in [3u32, 4, 5, 100] {
        assert_eq!(ekey(m, 0, class, 512).st, STATUS_INVALID_PARAMETER);
    }
    NtCloseAll(&[m, x, sb, sc]);
}

#[test]
fn enumerate_value_key_lists_overlay_then_real_values_minus_tombstones() {
    isolate!();
    let (_g, f) = fixture();
    let m = f.open(r"M\Merge", NT_KEY_READ);
    let x = f.open(r"X\Merge", NT_KEY_READ);
    assert_eq!(value_names(m), ["A_new", "b", "C", "E"]);
    assert_eq!(eval(m, 4, 0, 64).st, STATUS_NO_MORE_ENTRIES);
    let vals = merged_values();
    for (i, v) in vals.iter().enumerate() {
        for vc in [
            ValueInfoClass::Basic,
            ValueInfoClass::Full,
            ValueInfoClass::Partial,
            ValueInfoClass::FullAlign64,
            ValueInfoClass::PartialAlign64,
        ] {
            let total = lay(512, |b| layout::write_value_info(vc, v, b)).rl;
            for len in lens(total) {
                let got = eval(m, i as u32, vc as u32, len);
                if i < 2 {
                    // Written by the layout.
                    let want = lay(len, |b| layout::write_value_info(vc, v, b));
                    assert_eq!(got, want, "{} {vc:?} len {len}", v.name);
                } else {
                    // A real value: the host's answer for it.
                    assert_eq!(
                        got,
                        eval(x, i as u32, vc as u32, len),
                        "{} {vc:?} len {len}",
                        v.name
                    );
                }
            }
        }
    }
    // Where the host's layout agrees with Windows', the overlay's entries match it too.
    for i in 0..2u32 {
        for class in [0u32, 2] {
            assert_eq!(
                eval(m, i, class, 512),
                eval(x, i, class, 512),
                "{i} {class}"
            );
        }
    }
    assert_eq!(eval(m, 0, 5, 512).st, STATUS_INVALID_PARAMETER);
    NtCloseAll(&[m, x]);
}

#[test]
fn query_value_key_takes_the_overlay_then_the_real_key() {
    isolate!();
    let (_g, f) = fixture();
    let m = f.open(r"M\Merge", NT_KEY_READ);
    let x = f.open(r"X\Merge", NT_KEY_READ);
    // Overlay values (one shadowing a real value in another case), then real ones.
    for (name, v) in [
        ("A_new", val("A_new", REG_SZ, &sz("overlay-a"))),
        ("a_NEW", val("A_new", REG_SZ, &sz("overlay-a"))),
        ("B", val("b", REG_DWORD, &dword(7))),
        ("b", val("b", REG_DWORD, &dword(7))),
    ] {
        for class in 0..5u32 {
            let vc = [
                ValueInfoClass::Basic,
                ValueInfoClass::Full,
                ValueInfoClass::Partial,
                ValueInfoClass::FullAlign64,
                ValueInfoClass::PartialAlign64,
            ][class as usize];
            let total = lay(512, |b| layout::write_value_info(vc, &v, b)).rl;
            for len in lens(total) {
                let want = lay(len, |b| layout::write_value_info(vc, &v, b));
                assert_eq!(qval(m, name, class, len), want, "{name} {vc:?} len {len}");
            }
        }
        // Wine echoes the name as asked for; Windows (and the layout) reports it as stored.
        if name == v.name {
            for class in [0u32, 2] {
                assert_eq!(
                    qval(m, name, class, 512),
                    qval(x, name, class, 512),
                    "{name}"
                );
            }
        }
    }
    for name in ["C", "e", "E"] {
        for class in 0..5u32 {
            for len in [0usize, 12, 16, 512] {
                assert_eq!(
                    qval(m, name, class, len),
                    qval(x, name, class, len),
                    "{name} {class} {len}"
                );
            }
        }
    }
    // Tombstoned, and missing everywhere.
    for name in ["D", "d", "Nowhere"] {
        assert_eq!(
            qval(m, name, 2, 512).st,
            STATUS_OBJECT_NAME_NOT_FOUND,
            "{name}"
        );
    }
    assert_eq!(qval(m, "C", 5, 512).st, STATUS_INVALID_PARAMETER);
    NtCloseAll(&[m, x]);
}

/// `NtQueryMultipleValueKey` into `len` bytes. Returns the status, the entries, the bytes, and
/// (BufferLength, RequiredBufferLength) as left by the call.
fn qmulti(h: isize, names: &[&str], len: usize) -> (i32, Vec<ValueEntry>, Vec<u8>, u32, u32) {
    let ws: Vec<Vec<u16>> = names.iter().map(|n| n.encode_utf16().collect()).collect();
    let uss: Vec<UnicodeString> = ws.iter().map(|w| us(w)).collect();
    let mut entries: Vec<KeyValueEntry> = uss
        .iter()
        .map(|u| KeyValueEntry {
            value_name: u,
            data_length: 0xAAAA,
            data_offset: 0xBBBB,
            ty: 0xCCCC,
        })
        .collect();
    let mut buf = vec![S; len + 16];
    let mut blen = len as u32;
    let mut req = NO_RL;
    let st = unsafe {
        NtQueryMultipleValueKey(
            h,
            entries.as_mut_ptr(),
            entries.len() as u32,
            buf.as_mut_ptr(),
            &mut blen,
            &mut req,
        )
    };
    assert!(buf[len..].iter().all(|&b| b == S), "wrote past the buffer");
    for (e, u) in entries.iter().zip(&uss) {
        assert_eq!(
            e.value_name, u as *const _,
            "the name pointer is left alone"
        );
    }
    buf.truncate(len);
    let es = entries
        .iter()
        .map(|e| ValueEntry {
            data_length: e.data_length,
            data_offset: e.data_offset,
            ty: e.ty,
        })
        .collect();
    (st, es, buf, blen, req)
}

#[test]
fn query_multiple_value_key_follows_the_layout() {
    isolate!();
    let (_g, f) = fixture();
    let m = f.open(r"M\Merge", NT_KEY_READ);
    let vals = merged_values();
    let (a_new, b, c, e) = (&vals[0], &vals[1], &vals[2], &vals[3]);
    let seed = ValueEntry {
        data_length: 0xAAAA,
        data_offset: 0xBBBB,
        ty: 0xCCCC,
    };
    let names = ["C", "A_new", "B", "e"];
    let found = [Some(c), Some(a_new), Some(b), Some(e)];
    for len in [0usize, 3, 4, 8, 27, 28, 31, 32, 64] {
        let mut want_entries = vec![seed; 4];
        let mut want_buf = vec![S; len];
        let w = layout::write_multiple_values(&found, &mut want_entries, &mut want_buf);
        let (st, entries, buf, blen, req) = qmulti(m, &names, len);
        assert_eq!(st, w.status, "len {len}");
        assert_eq!(entries, want_entries, "len {len}");
        assert_eq!(buf, want_buf, "len {len}");
        assert_eq!((blen, req), (w.buffer_length, w.result_length), "len {len}");
    }
    // A tombstoned name: not found, the lengths left as they were.
    let (st, entries, buf, blen, req) = qmulti(m, &["C", "D", "A_new"], 64);
    assert_eq!(st, STATUS_OBJECT_NAME_NOT_FOUND);
    assert_eq!((blen, req), (64, NO_RL));
    assert_eq!(&buf[..4], &dword(3)[..]);
    assert_eq!(entries[0].data_length, 4);
    assert_eq!(&entries[1..], &[seed, seed]);
    NtCloseAll(&[m]);
}

#[test]
fn a_key_created_again_hides_the_real_contents() {
    isolate!();
    let (_g, f) = fixture();
    let r = f.open("Revived", NT_KEY_READ);
    assert!(is_synthetic_key_handle(r));
    assert_eq!(subkey_names(r), ["New"]);
    assert_eq!(value_names(r), ["n"]);
    assert_eq!(qval(r, "v", 2, 64).st, STATUS_OBJECT_NAME_NOT_FOUND);
    // Full: one subkey, one value, no class (the real "RCls" is hidden).
    let a = qkey(r, 2, 64);
    assert_eq!(a.st, STATUS_SUCCESS);
    let u = |o: usize| u32::from_le_bytes(a.bytes[o..o + 4].try_into().unwrap());
    assert_eq!(
        (u(12), u(16), u(20), u(28), u(32)),
        (u32::MAX, 0, 1, 0, 1),
        "ClassOffset, ClassLength, SubKeys, MaxClassLen, Values"
    );
    NtCloseAll(&[r]);
}

#[test]
fn a_handle_to_a_key_deleted_in_the_overlay_reports_it_deleted() {
    isolate!();
    let (_g, f) = fixture();
    let d = f.open("Doomed", NT_KEY_READ);
    assert!(is_synthetic_key_handle(d));
    assert_eq!(qkey(d, 0, 64).st, STATUS_SUCCESS);
    regclient::delete_key(&f.canon("Doomed")).unwrap();
    assert_eq!(qkey(d, 0, 64).st, STATUS_KEY_DELETED);
    assert_eq!(ekey(d, 0, 0, 64).st, STATUS_KEY_DELETED);
    assert_eq!(qval(d, "o", 2, 64).st, STATUS_KEY_DELETED);
    assert_eq!(eval(d, 0, 0, 64).st, STATUS_KEY_DELETED);
    NtCloseAll(&[d]);
}

#[test]
fn a_pass_through_handle_is_the_real_key_until_its_path_is_touched() {
    isolate!();
    let (_g, f) = fixture();
    // Untouched: the real key, unchanged.
    let before = registry_enum_states();
    let p = f.open("Plain", NT_KEY_READ);
    assert!(!is_synthetic_key_handle(p));
    assert_eq!(subkey_names(p), ["PSub"]);
    assert_eq!(value_names(p), ["p"]);
    let a = qval(p, "p", 2, 64);
    assert_eq!(
        (a.st, &a.bytes[8..16]),
        (STATUS_SUCCESS, &[4, 0, 0, 0, 5, 0, 0, 0][..])
    );
    assert_eq!(
        registry_enum_states(),
        before,
        "no snapshot for an untouched pass-through handle"
    );
    NtCloseAll(&[p]);

    // Touched after the open (Task 11's copy-on-write writes through such a handle): the same
    // handle now answers from the merge, decided on every call.
    let c = f.open("Cow", NT_KEY_READ);
    assert!(!is_synthetic_key_handle(c));
    assert_eq!(value_names(c), ["c"]);
    regclient::set_value(&f.canon("Cow"), "Added", REG_DWORD, &dword(3)).unwrap();
    assert_eq!(value_names(c), ["Added", "c"]);
    assert_eq!(qval(c, "added", 2, 64).bytes[12..16], dword(3)[..]);
    let full = qkey(c, 2, 64);
    assert_eq!(
        u32::from_le_bytes(full.bytes[32..36].try_into().unwrap()),
        2
    );
    NtCloseAll(&[c]);

    // A handle without the right to list subkeys still gets the merged counts: the merge reads
    // the real key through a private handle.
    let q = f.open("CowQ", KEY_QUERY_VALUE);
    assert!(!is_synthetic_key_handle(q));
    regclient::set_value(&f.canon("CowQ"), "Added", REG_DWORD, &dword(3)).unwrap();
    let full = qkey(q, 2, 64);
    assert_eq!(full.st, STATUS_SUCCESS);
    let u = |o: usize| u32::from_le_bytes(full.bytes[o..o + 4].try_into().unwrap());
    assert_eq!((u(20), u(32)), (1, 2), "SubKeys, Values");
    NtCloseAll(&[q]);
}

#[test]
fn a_synthetic_handle_needs_the_right_for_each_query() {
    isolate!();
    let (_g, f) = fixture();
    let e = f.open("Access", KEY_ENUMERATE_SUB_KEYS);
    assert!(is_synthetic_key_handle(e));
    assert_eq!(subkey_names(e), ["Sub"]);
    assert_eq!(qkey(e, 0, 64).st, STATUS_ACCESS_DENIED);
    assert_eq!(qkey(e, 2, 64).st, STATUS_ACCESS_DENIED);
    assert_eq!(
        qkey(e, 3, 512).st,
        STATUS_SUCCESS,
        "the name needs no right"
    );
    assert_eq!(qval(e, "o", 2, 64).st, STATUS_ACCESS_DENIED);
    assert_eq!(eval(e, 0, 0, 64).st, STATUS_ACCESS_DENIED);
    // The class is checked before the access, as Windows does.
    assert_eq!(eval(e, 0, 5, 64).st, STATUS_INVALID_PARAMETER);
    assert_eq!(qval(e, "o", 5, 64).st, STATUS_INVALID_PARAMETER);
    assert_eq!(qmulti(e, &["o"], 64).0, STATUS_ACCESS_DENIED);
    let q = f.open("Access", KEY_QUERY_VALUE);
    assert_eq!(ekey(q, 0, 0, 64).st, STATUS_ACCESS_DENIED);
    assert_eq!(value_names(q), ["o", "r"]);
    assert_eq!(qkey(q, 0, 64).st, STATUS_SUCCESS);
    NtCloseAll(&[e, q]);
}

#[test]
fn enumeration_sees_live_overlay_changes_and_close_drops_its_snapshot() {
    isolate!();
    let (_g, f) = fixture();
    let before = registry_enum_states();
    let l = f.open("Live", NT_KEY_READ);
    let name = |i: u32| {
        let a = ekey(l, i, 0, 64);
        (a.st == STATUS_SUCCESS).then(|| utf16(&a.bytes[16..16 + a.bytes[12] as usize]))
    };
    assert_eq!(name(0).as_deref(), Some("A"));
    assert_eq!(registry_enum_states(), before + 1);
    // Created mid-enumeration: listed after the real ones, at once.
    regclient::create_key(&f.canon(r"Live\C"), false).unwrap();
    assert_eq!(name(1).as_deref(), Some("B"));
    assert_eq!(name(2).as_deref(), Some("C"));
    // Deleted mid-enumeration: later keys move down, as on Windows.
    regclient::delete_key(&f.canon(r"Live\A")).unwrap();
    assert_eq!(name(2), None);
    assert_eq!(name(1).as_deref(), Some("C"));
    // Index 0 starts afresh.
    assert_eq!(name(0).as_deref(), Some("B"));
    NtCloseAll(&[l]);
    assert_eq!(registry_enum_states(), before, "closed with its handle");
}

#[test]
fn a_failing_director_answers_from_the_real_key() {
    isolate!();
    let (_g, f) = fixture();
    let d = f.open("Down", NT_KEY_READ);
    let e = f.open("Down", KEY_ENUMERATE_SUB_KEYS);
    assert!(is_synthetic_key_handle(d));
    assert_eq!(value_names(d), ["o", "r"]);
    let host = f.fake.director().registry().unwrap();
    f.fake.director().set_registry(None);
    let names = value_names(d);
    let overlay_value = qval(d, "o", 2, 64).st;
    // The real key's answer is the private handle's, but the access is still the caller's.
    let denied = qval(e, "r", 2, 64).st;
    f.fake.director().set_registry(Some(host));
    assert_eq!(names, ["r"], "the real key alone");
    assert_eq!(overlay_value, STATUS_OBJECT_NAME_NOT_FOUND);
    assert_eq!(denied, STATUS_ACCESS_DENIED);
    NtCloseAll(&[d, e]);
}

#[test]
fn a_handle_opened_before_the_hooks_is_merged_too() {
    isolate!();
    let (_g, f) = fixture();
    // kernelbase's HKEY_CURRENT_USER is a real handle opened before the hooks went in: neither
    // table knows it, so its path comes from the real key's name.
    let root = r"\Registry\User\<CurrentUser>";
    regclient::set_value(root, "AetherVfsRegQueryRoot", REG_DWORD, &dword(42)).unwrap();
    let mut ty = 0u32;
    let mut data = [0u8; 4];
    let mut n = 4u32;
    let st = unsafe {
        RegQueryValueExW(
            HKEY_CURRENT_USER,
            wide("AetherVfsRegQueryRoot").as_ptr(),
            std::ptr::null(),
            &mut ty,
            data.as_mut_ptr(),
            &mut n,
        )
    };
    regclient::delete_value(root, "AetherVfsRegQueryRoot").unwrap();
    assert_eq!((st, ty, u32::from_le_bytes(data)), (0, REG_DWORD, 42));
    let _ = f;
}

#[test]
fn a_node_too_large_to_read_enumerates_the_real_key_alone() {
    isolate!();
    let (_g, f) = fixture();
    let h = f.open("TooBig", NT_KEY_READ);
    assert!(is_synthetic_key_handle(h));
    let states = registry_enum_states();
    let fallbacks = vfs_shim::reg_read_fallback_count();
    assert_eq!(subkey_names(h), ["R1", "R2"]);
    assert_eq!(value_names(h), ["r"]);
    assert_eq!(
        registry_enum_states(),
        states,
        "a fallback list is not kept"
    );
    assert!(
        vfs_shim::reg_read_fallback_count() > fallbacks,
        "fallbacks are counted"
    );
    assert_eq!(qval(h, "r", 2, 64).bytes[12..16], dword(1)[..]);
    NtCloseAll(&[h]);
}

#[test]
fn a_handle_opened_before_the_hooks_is_recorded_with_its_granted_access() {
    isolate!();
    let (_g, f) = fixture();
    // First sight: resolved from its name, recorded as pass-through with the kernel's access.
    assert_eq!(vfs_shim::registry_handle_path(f.pre), None);
    assert_eq!(subkey_names(f.pre), ["PreSub", "Added"]);
    assert_eq!(vfs_shim::registry_handle_path(f.pre), Some(f.canon("Pre")));
    assert_eq!(qval(f.pre, "p", 2, 64).st, STATUS_ACCESS_DENIED);
    assert_eq!(qkey(f.pre, 0, 64).st, STATUS_ACCESS_DENIED);

    // Not a key: remembered, so it costs no syscall next time, and forgotten on close.
    use windows_sys::Win32::System::Threading::CreateEventW;
    let ev = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) } as isize;
    let before = vfs_shim::registry_not_ours_count();
    let first = qkey(ev, 0, 64).st;
    assert!(first < 0, "{first:#x}");
    assert_eq!(vfs_shim::registry_not_ours_count(), before + 1);
    assert_eq!(qkey(ev, 0, 64).st, first);
    assert_eq!(vfs_shim::registry_not_ours_count(), before + 1);
    NtCloseAll(&[ev]);
    assert_eq!(vfs_shim::registry_not_ours_count(), before);
}

#[test]
fn full_needs_only_query_value_even_when_the_key_refuses_key_read() {
    isolate!();
    let (_g, f) = fixture();
    assert!(
        f.limited_as_expected,
        "the DACL did not refuse KEY_READ (or refused KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS)"
    );
    let h = f.open("Limited", KEY_QUERY_VALUE);
    assert!(is_synthetic_key_handle(h));
    let full = qkey(h, 2, 64);
    assert_eq!(full.st, STATUS_SUCCESS);
    let u = |o: usize| u32::from_le_bytes(full.bytes[o..o + 4].try_into().unwrap());
    assert_eq!((u(20), u(32)), (1, 2), "SubKeys, Values");
    assert_eq!(qkey(h, 4, 64).st, STATUS_SUCCESS);
    NtCloseAll(&[h]);
}
