//! NT information-class layouts for `NtQueryKey`, `NtEnumerateKey`, `NtQueryValueKey`,
//! `NtEnumerateValueKey` and `NtQueryMultipleValueKey`, written as explicit little-endian
//! bytes at the documented offsets (x64 layouts).
//!
//! Buffer rules follow the NT configuration manager (WRK `CmpQueryKeyData`,
//! `CmpQueryKeyDataFromCache`, `CmpQueryKeyValueData`, `CmQueryKey`, `CmQueryMultipleValueKey`):
//! - `result_length` is always the full size;
//! - a buffer shorter than the fixed part gives `STATUS_BUFFER_TOO_SMALL` and nothing is written;
//! - otherwise the fixed part is written with the full lengths, as much of each variable part as
//!   fits is copied, and a short buffer gives `STATUS_BUFFER_OVERFLOW`;
//! - bytes the structure does not define (alignment padding) are left as the caller had them.

use crate::merge::MergedKey;
use crate::overlay::Value;

pub const STATUS_SUCCESS: i32 = 0;
pub const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005_u32 as i32;
pub const STATUS_INVALID_PARAMETER: i32 = 0xC000_000D_u32 as i32;
pub const STATUS_BUFFER_TOO_SMALL: i32 = 0xC000_0023_u32 as i32;
pub const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034_u32 as i32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum KeyInfoClass {
    Basic = 0,
    Node = 1,
    Full = 2,
    Name = 3,
    Cached = 4,
    Flags = 5,
    Virtualization = 6,
    HandleTags = 7,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ValueInfoClass {
    Basic = 0,
    Full = 1,
    Partial = 2,
    FullAlign64 = 3,
    PartialAlign64 = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    pub status: i32,
    pub result_length: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ValueEntry {
    pub data_length: u32,
    pub data_offset: u32,
    pub ty: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultipleWritten {
    pub status: i32,
    pub buffer_length: u32,
    pub result_length: u32,
}

/// Little-endian writes at fixed offsets. Callers only write the fixed part after checking the
/// buffer holds it; `tail` copies as much of a variable part as fits.
struct Out<'a>(&'a mut [u8]);

impl Out<'_> {
    fn u32(&mut self, off: usize, v: u32) {
        self.0[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, off: usize, v: u64) {
        self.0[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    /// Copy `bytes` at `off`, truncated to the buffer. Returns whether it all fit.
    fn tail(&mut self, off: usize, bytes: &[u8]) -> bool {
        let room = self.0.len().saturating_sub(off);
        let k = bytes.len().min(room);
        if k > 0 {
            self.0[off..off + k].copy_from_slice(&bytes[..k]);
        }
        k == bytes.len()
    }
}

fn len32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn utf16_bytes(s: &str) -> u32 {
    len32(s.encode_utf16().count() * 2)
}

fn class_bytes(k: &MergedKey) -> Vec<u8> {
    k.class
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .flat_map(|u| u.to_le_bytes())
        .collect()
}

/// The key's own name: the last component of its NT path.
fn leaf(path: &str) -> &str {
    path.rsplit('\\').next().unwrap_or(path)
}

fn align(n: usize, to: usize) -> usize {
    n.div_ceil(to) * to
}

fn status(fit: bool) -> i32 {
    if fit {
        STATUS_SUCCESS
    } else {
        STATUS_BUFFER_OVERFLOW
    }
}

/// Counts and maxima over the merged view, in the units NT reports (bytes for lengths).
struct Counts {
    subkeys: u32,
    max_name: u32,
    max_class: u32,
    values: u32,
    max_value_name: u32,
    max_value_data: u32,
}

fn counts(k: &MergedKey) -> Counts {
    Counts {
        subkeys: len32(k.subkeys.len()),
        max_name: k.subkeys.iter().map(|s| utf16_bytes(s)).max().unwrap_or(0),
        max_class: k.max_subkey_class_len,
        values: len32(k.values.len()),
        max_value_name: k
            .values
            .iter()
            .map(|v| utf16_bytes(&v.name))
            .max()
            .unwrap_or(0),
        max_value_data: k
            .values
            .iter()
            .map(|v| len32(v.data.len()))
            .max()
            .unwrap_or(0),
    }
}

fn too_small(result_length: usize) -> Written {
    Written {
        status: STATUS_BUFFER_TOO_SMALL,
        result_length: len32(result_length),
    }
}

/// KEY_BASIC_INFORMATION: LastWriteTime@0 TitleIndex@8 NameLength@12 Name@16.
fn key_basic(name: &str, k: &MergedKey, buf: &mut [u8]) -> Written {
    let name = utf16le(name);
    let total = 16 + name.len();
    if buf.len() < 16 {
        return too_small(total);
    }
    let mut o = Out(buf);
    o.u64(0, k.last_write);
    o.u32(8, 0);
    o.u32(12, len32(name.len()));
    let fit = o.tail(16, &name);
    Written {
        status: status(fit),
        result_length: len32(total),
    }
}

/// KEY_NODE_INFORMATION: LastWriteTime@0 TitleIndex@8 ClassOffset@12 ClassLength@16
/// NameLength@20 Name@24; the class at ALIGN4(24 + NameLength), or ClassOffset = -1 without one.
/// ResultLength is 24 + NameLength + ClassLength, without the alignment padding (WRK
/// `CmpQueryKeyData`), so a buffer of exactly ResultLength can overflow by 2 bytes.
fn key_node(name: &str, k: &MergedKey, buf: &mut [u8]) -> Written {
    let name = utf16le(name);
    let class = class_bytes(k);
    let total = 24 + name.len() + class.len();
    if buf.len() < 24 {
        return too_small(total);
    }
    let mut o = Out(buf);
    o.u64(0, k.last_write);
    o.u32(8, 0);
    o.u32(16, len32(class.len()));
    o.u32(20, len32(name.len()));
    let mut fit = o.tail(24, &name);
    if class.is_empty() {
        o.u32(12, u32::MAX);
    } else {
        let off = align(24 + name.len(), 4);
        o.u32(12, len32(off));
        fit &= o.tail(off, &class);
    }
    Written {
        status: status(fit),
        result_length: len32(total),
    }
}

/// KEY_FULL_INFORMATION: LastWriteTime@0 TitleIndex@8 ClassOffset@12 ClassLength@16 SubKeys@20
/// MaxNameLen@24 MaxClassLen@28 Values@32 MaxValueNameLen@36 MaxValueDataLen@40 Class@44.
fn key_full(k: &MergedKey, buf: &mut [u8]) -> Written {
    let class = class_bytes(k);
    let total = 44 + class.len();
    if buf.len() < 44 {
        return too_small(total);
    }
    let c = counts(k);
    let mut o = Out(buf);
    o.u64(0, k.last_write);
    o.u32(8, 0);
    o.u32(12, if class.is_empty() { u32::MAX } else { 44 });
    o.u32(16, len32(class.len()));
    o.u32(20, c.subkeys);
    o.u32(24, c.max_name);
    o.u32(28, c.max_class);
    o.u32(32, c.values);
    o.u32(36, c.max_value_name);
    o.u32(40, c.max_value_data);
    let fit = o.tail(44, &class);
    Written {
        status: status(fit),
        result_length: len32(total),
    }
}

/// KEY_NAME_INFORMATION: NameLength@0 Name@4 (the full NT path). On overflow WRK `CmQueryKey`
/// copies as much of the name as fits.
fn key_name(path: &str, buf: &mut [u8]) -> Written {
    let name = utf16le(path);
    let total = 4 + name.len();
    if buf.len() < 4 {
        return too_small(total);
    }
    let mut o = Out(buf);
    o.u32(0, len32(name.len()));
    let fit = o.tail(4, &name);
    Written {
        status: status(fit),
        result_length: len32(total),
    }
}

/// KEY_CACHED_INFORMATION, sizeof 40: LastWriteTime@0 TitleIndex@8 SubKeys@12 MaxNameLen@16
/// Values@20 MaxValueNameLen@24 MaxValueDataLen@28 NameLength@32, padding 36..40. The name is
/// not copied (WRK `CmpQueryKeyDataFromCache`), so a short buffer is only ever too small.
fn key_cached(path: &str, k: &MergedKey, buf: &mut [u8]) -> Written {
    if buf.len() < 40 {
        return too_small(40);
    }
    let c = counts(k);
    let mut o = Out(buf);
    o.u64(0, k.last_write);
    o.u32(8, 0);
    o.u32(12, c.subkeys);
    o.u32(16, c.max_name);
    o.u32(20, c.values);
    o.u32(24, c.max_value_name);
    o.u32(28, c.max_value_data);
    o.u32(32, utf16_bytes(leaf(path)));
    Written {
        status: STATUS_SUCCESS,
        result_length: 40,
    }
}

/// A fixed-size class with nothing set for an overlay key: KEY_FLAGS_INFORMATION (12: Wow64Flags,
/// KeyFlags, ControlFlags), KEY_VIRTUALIZATION_INFORMATION (4: bitfield) and
/// KEY_HANDLE_TAGS_INFORMATION (4: HandleTags).
fn key_zeroed(size: usize, buf: &mut [u8]) -> Written {
    if buf.len() < size {
        return too_small(size);
    }
    buf[..size].fill(0);
    Written {
        status: STATUS_SUCCESS,
        result_length: len32(size),
    }
}

/// `NtQueryKey` on a key. `name_for_name_class` is the key's full NT path: KeyNameInformation
/// returns it, and Basic, Node and Cached report its last component, the key's own name.
pub fn write_key_info(
    class: KeyInfoClass,
    key: &MergedKey,
    name_for_name_class: &str,
    buf: &mut [u8],
) -> Written {
    match class {
        KeyInfoClass::Basic => key_basic(leaf(name_for_name_class), key, buf),
        KeyInfoClass::Node => key_node(leaf(name_for_name_class), key, buf),
        KeyInfoClass::Full => key_full(key, buf),
        KeyInfoClass::Name => key_name(name_for_name_class, buf),
        KeyInfoClass::Cached => key_cached(name_for_name_class, key, buf),
        KeyInfoClass::Flags => key_zeroed(12, buf),
        KeyInfoClass::Virtualization | KeyInfoClass::HandleTags => key_zeroed(4, buf),
    }
}

/// `NtEnumerateKey`: subkey `name` with its own merged view `sub`. Only Basic, Node and Full are
/// valid here (WRK `CmpQueryKeyData`); any other class is `STATUS_INVALID_PARAMETER` with
/// nothing written.
pub fn write_subkey_info(
    class: KeyInfoClass,
    name: &str,
    sub: &MergedKey,
    buf: &mut [u8],
) -> Written {
    match class {
        KeyInfoClass::Basic => key_basic(name, sub, buf),
        KeyInfoClass::Node => key_node(name, sub, buf),
        KeyInfoClass::Full => key_full(sub, buf),
        _ => Written {
            status: STATUS_INVALID_PARAMETER,
            result_length: 0,
        },
    }
}

/// `NtQueryValueKey` / `NtEnumerateValueKey` for one value (WRK `CmpQueryKeyValueData`).
pub fn write_value_info(class: ValueInfoClass, v: &Value, buf: &mut [u8]) -> Written {
    let data = v.data.as_slice();
    match class {
        // KEY_VALUE_BASIC_INFORMATION: TitleIndex@0 Type@4 NameLength@8 Name@12.
        ValueInfoClass::Basic => {
            let name = utf16le(&v.name);
            let total = 12 + name.len();
            if buf.len() < 12 {
                return too_small(total);
            }
            let mut o = Out(buf);
            o.u32(0, 0);
            o.u32(4, v.ty);
            o.u32(8, len32(name.len()));
            let fit = o.tail(12, &name);
            Written {
                status: status(fit),
                result_length: len32(total),
            }
        }
        // KEY_VALUE_FULL_INFORMATION: TitleIndex@0 Type@4 DataOffset@8 DataLength@12
        // NameLength@16 Name@20. With data, the data is at ALIGN8(20 + NameLength) for both
        // Full and FullAlign64: the x64 kernel aligns both to 8 (WRK `_WIN64` branch). Without
        // data, DataOffset is -1 and there is no padding.
        ValueInfoClass::Full | ValueInfoClass::FullAlign64 => {
            let name = utf16le(&v.name);
            let base = 20 + name.len();
            let off = if data.is_empty() {
                base
            } else {
                align(base, 8)
            };
            let total = off + data.len();
            if buf.len() < 20 {
                return too_small(total);
            }
            let mut o = Out(buf);
            o.u32(0, 0);
            o.u32(4, v.ty);
            o.u32(12, len32(data.len()));
            o.u32(16, len32(name.len()));
            let mut fit = o.tail(20, &name);
            if data.is_empty() {
                o.u32(8, u32::MAX);
            } else {
                o.u32(8, len32(off));
                fit &= o.tail(off, data);
            }
            Written {
                status: status(fit),
                result_length: len32(total),
            }
        }
        // KEY_VALUE_PARTIAL_INFORMATION: TitleIndex@0 Type@4 DataLength@8 Data@12.
        ValueInfoClass::Partial => {
            let total = 12 + data.len();
            if buf.len() < 12 {
                return too_small(total);
            }
            let mut o = Out(buf);
            o.u32(0, 0);
            o.u32(4, v.ty);
            o.u32(8, len32(data.len()));
            let fit = o.tail(12, data);
            Written {
                status: status(fit),
                result_length: len32(total),
            }
        }
        // KEY_VALUE_PARTIAL_INFORMATION_ALIGN64: Type@0 DataLength@4 Data@8 (no TitleIndex).
        ValueInfoClass::PartialAlign64 => {
            let total = 8 + data.len();
            if buf.len() < 8 {
                return too_small(total);
            }
            let mut o = Out(buf);
            o.u32(0, v.ty);
            o.u32(4, len32(data.len()));
            let fit = o.tail(8, data);
            Written {
                status: status(fit),
                result_length: len32(total),
            }
        }
    }
}

/// `NtQueryMultipleValueKey`, following WRK `CmQueryMultipleValueKey`:
/// - each value's data goes at the next ULONG (4-byte) aligned offset in `buf`;
/// - once one value does not fit, the status is `STATUS_BUFFER_OVERFLOW` and no later value is
///   copied or has its entry filled, even if it would fit; `result_length` keeps counting;
/// - `entries[i]` is set only for a copied value; the others are left as passed in, so the shim
///   seeds `entries` from the caller's array and writes them all back;
/// - a `None` (value not found) stops at once with `STATUS_OBJECT_NAME_NOT_FOUND`; earlier
///   values stay copied, and the caller's `*BufferLength` and `*RequiredBufferLength` are left
///   unchanged.
///
/// On success or overflow the shim writes `buffer_length` (bytes used) to `*BufferLength` and
/// `result_length` (bytes required) to `*RequiredBufferLength`.
pub fn write_multiple_values(
    values: &[Option<&Value>],
    entries: &mut [ValueEntry],
    buf: &mut [u8],
) -> MultipleWritten {
    debug_assert_eq!(values.len(), entries.len(), "one entry per value");
    let mut used = 0usize;
    let mut required = 0usize;
    let mut full = false;
    let mut status = STATUS_SUCCESS;
    for (v, entry) in values.iter().zip(entries.iter_mut()) {
        let Some(v) = v else {
            return MultipleWritten {
                status: STATUS_OBJECT_NAME_NOT_FOUND,
                buffer_length: len32(used),
                result_length: len32(required),
            };
        };
        let len = v.data.len();
        used = align(used, 4);
        required = align(required, 4);
        if !full && used + len <= buf.len() {
            buf[used..used + len].copy_from_slice(&v.data);
            *entry = ValueEntry {
                data_length: len32(len),
                data_offset: len32(used),
                ty: v.ty,
            };
            used += len;
        } else {
            full = true;
            status = STATUS_BUFFER_OVERFLOW;
        }
        required += len;
    }
    MultipleWritten {
        status,
        buffer_length: len32(used),
        result_length: len32(required),
    }
}

/// sizeof(KEY_VALUE_ENTRY) on x64: ValueName@0 (pointer) DataLength@8 DataOffset@12 Type@16.
pub const KEY_VALUE_ENTRY_SIZE: usize = 24;

impl ValueEntry {
    /// Write this entry into a caller's x64 KEY_VALUE_ENTRY slot, leaving ValueName unchanged.
    pub fn store(&self, slot: &mut [u8]) {
        let mut o = Out(slot);
        o.u32(8, self.data_length);
        o.u32(12, self.data_offset);
        o.u32(16, self.ty);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::{merge, RealKey};
    use crate::overlay::Overlay;

    /// Sentinel for bytes the call must not touch.
    const S: u8 = 0xCC;
    const LW: u64 = 0x01D9_0000_1234_5678;
    const LW_BYTES: [u8; 8] = [0x78, 0x56, 0x34, 0x12, 0x00, 0x00, 0xD9, 0x01];
    const NONE: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];
    const ZERO: [u8; 4] = [0, 0, 0, 0];
    const KEY: &str = r"\REGISTRY\MACHINE\SOFTWARE\Mod";

    fn n(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }
    fn w(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }
    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }
    fn pad(k: usize) -> Vec<u8> {
        vec![S; k]
    }

    /// Run `f` on the first `len` bytes of a sentinel-filled buffer that is 16 bytes longer, and
    /// check nothing past `len` was touched.
    fn run(len: usize, f: impl FnOnce(&mut [u8]) -> Written) -> (Written, Vec<u8>) {
        let mut v = vec![S; len + 16];
        let r = f(&mut v[..len]);
        assert!(v[len..].iter().all(|&b| b == S), "wrote past the buffer");
        v.truncate(len);
        (r, v)
    }
    fn ok(len: u32) -> Written {
        Written {
            status: STATUS_SUCCESS,
            result_length: len,
        }
    }
    fn overflow(len: u32) -> Written {
        Written {
            status: STATUS_BUFFER_OVERFLOW,
            result_length: len,
        }
    }
    fn too_small(len: u32) -> Written {
        Written {
            status: STATUS_BUFFER_TOO_SMALL,
            result_length: len,
        }
    }
    fn key(class: Option<&str>) -> MergedKey {
        MergedKey {
            last_write: LW,
            class: class.map(|c| c.encode_utf16().collect()),
            ..MergedKey::default()
        }
    }
    fn val(name: &str, ty: u32, data: &[u8]) -> Value {
        Value {
            name: name.into(),
            ty,
            data: data.into(),
        }
    }
    fn kq(class: KeyInfoClass, k: &MergedKey, len: usize) -> (Written, Vec<u8>) {
        run(len, |b| write_key_info(class, k, KEY, b))
    }
    fn vq(class: ValueInfoClass, v: &Value, len: usize) -> (Written, Vec<u8>) {
        run(len, |b| write_value_info(class, v, b))
    }

    // ---- KEY_BASIC_INFORMATION: LastWriteTime@0 TitleIndex@8 NameLength@12 Name@16 ----

    fn basic_mod() -> Vec<u8> {
        cat(&[&LW_BYTES, &ZERO, &n(6), &w("Mod")])
    }

    #[test]
    fn key_basic_exact() {
        let (r, b) = kq(KeyInfoClass::Basic, &key(None), 22);
        assert_eq!(r, ok(22));
        assert_eq!(b, basic_mod());
    }

    #[test]
    fn key_basic_overflow_copies_part_of_the_name() {
        let (r, b) = kq(KeyInfoClass::Basic, &key(None), 19);
        assert_eq!(r, overflow(22));
        assert_eq!(b, basic_mod()[..19]);
        // Exactly the fixed part: header only.
        let (r, b) = kq(KeyInfoClass::Basic, &key(None), 16);
        assert_eq!(r, overflow(22));
        assert_eq!(b, basic_mod()[..16]);
    }

    #[test]
    fn key_basic_too_small_writes_nothing() {
        for len in [0, 1, 15] {
            let (r, b) = kq(KeyInfoClass::Basic, &key(None), len);
            assert_eq!(r, too_small(22));
            assert_eq!(b, pad(len));
        }
    }

    // ---- KEY_NODE_INFORMATION: LastWriteTime@0 TitleIndex@8 ClassOffset@12 ClassLength@16
    //      NameLength@20 Name@24, Class at ALIGN4(24 + NameLength) ----

    #[test]
    fn key_node_without_class() {
        let (r, b) = kq(KeyInfoClass::Node, &key(None), 30);
        assert_eq!(r, ok(30));
        assert_eq!(b, cat(&[&LW_BYTES, &ZERO, &NONE, &ZERO, &n(6), &w("Mod")]));
    }

    fn node_mod_abc() -> Vec<u8> {
        // Name ends at 30; the class is at 32; bytes 30..32 are padding the call leaves alone.
        cat(&[
            &LW_BYTES,
            &ZERO,
            &n(32),
            &n(6),
            &n(6),
            &w("Mod"),
            &pad(2),
            &w("ABC"),
        ])
    }

    #[test]
    fn key_node_with_class_is_ulong_aligned() {
        let (r, b) = kq(KeyInfoClass::Node, &key(Some("ABC")), 38);
        // ResultLength does not count the padding (WRK: 24 + NameLength + ClassLength).
        assert_eq!(r, ok(36));
        assert_eq!(b, node_mod_abc());
    }

    #[test]
    fn key_node_buffer_of_result_length_overflows_by_the_padding() {
        // Windows quirk: a buffer of exactly ResultLength is 2 bytes short of the aligned class.
        let (r, b) = kq(KeyInfoClass::Node, &key(Some("ABC")), 36);
        assert_eq!(r, overflow(36));
        assert_eq!(b, node_mod_abc()[..36]);
    }

    #[test]
    fn key_node_overflow_cases() {
        let k = key(Some("ABC"));
        // Name partly copied; class offset and lengths still the full values; no class bytes.
        let (r, b) = kq(KeyInfoClass::Node, &k, 27);
        assert_eq!(r, overflow(36));
        assert_eq!(b, node_mod_abc()[..27]);
        // Ends inside the padding: nothing of the class.
        let (r, b) = kq(KeyInfoClass::Node, &k, 31);
        assert_eq!(r, overflow(36));
        assert_eq!(b, node_mod_abc()[..31]);
        // Fixed part only.
        let (r, b) = kq(KeyInfoClass::Node, &k, 24);
        assert_eq!(r, overflow(36));
        assert_eq!(b, node_mod_abc()[..24]);
    }

    #[test]
    fn key_node_name_already_aligned_has_no_padding() {
        let (r, b) = run(32, |b| {
            write_key_info(KeyInfoClass::Node, &key(Some("Z")), r"\REGISTRY\Ab", b)
        });
        assert_eq!(r, ok(30));
        assert_eq!(
            b,
            cat(&[
                &LW_BYTES,
                &ZERO,
                &n(28),
                &n(2),
                &n(4),
                &w("Ab"),
                &w("Z"),
                &pad(2)
            ])
        );
    }

    #[test]
    fn key_node_too_small() {
        for len in [0, 23] {
            let (r, b) = kq(KeyInfoClass::Node, &key(Some("ABC")), len);
            assert_eq!(r, too_small(36));
            assert_eq!(b, pad(len));
        }
    }

    // ---- KEY_FULL_INFORMATION: LastWriteTime@0 TitleIndex@8 ClassOffset@12 ClassLength@16
    //      SubKeys@20 MaxNameLen@24 MaxClassLen@28 Values@32 MaxValueNameLen@36
    //      MaxValueDataLen@40 Class@44 ----

    const P: &str = r"\Registry\Machine\Software\Mod";

    /// A key whose counts depend on the merge: a tombstoned real subkey and value, an overlay
    /// subkey with the longest name, an overlay value shadowing nothing.
    fn merged() -> MergedKey {
        let real = RealKey {
            subkeys: vec!["alphabetical".into(), "Be".into()],
            values: vec![
                val("x", 1, &[1, 2, 3]),
                val("LongestValueName", 3, &[0; 100]),
            ],
            class: Some("CL".encode_utf16().collect()),
            last_write: LW,
            max_subkey_class_len: 8,
        };
        let mut o = Overlay::new();
        o.delete_key(&format!(r"{P}\ALPHABETICAL"), LW - 1).unwrap();
        o.create_key(&format!(r"{P}\LongerName12"), false, false, LW - 1)
            .unwrap();
        o.delete_value(P, "longestvaluename", LW - 1).unwrap();
        o.set_value(P, "Value", 4, &[9; 10], LW - 1).unwrap();
        merge(Some(&real), o.node(P), false).unwrap()
    }

    #[test]
    fn key_full_counts_and_maxima_over_the_merged_view() {
        let m = merged();
        assert_eq!(m.subkeys, vec!["Be", "LongerName12"]);
        let (r, b) = kq(KeyInfoClass::Full, &m, 48);
        assert_eq!(r, ok(48));
        let expected = cat(&[
            &LW_BYTES,
            &ZERO,
            &n(44), // ClassOffset
            &n(4),  // ClassLength
            &n(2),  // SubKeys: Be, LongerName12 (alphabetical tombstoned)
            &n(24), // MaxNameLen: "LongerName12" in bytes
            &n(8),  // MaxClassLen: carried from the real key
            &n(2),  // Values: Value, x (LongestValueName tombstoned)
            &n(10), // MaxValueNameLen: "Value"
            &n(10), // MaxValueDataLen: Value's 10 bytes, not the hidden 100
            &w("CL"),
        ]);
        assert_eq!(b, expected);
    }

    fn full_plain() -> Vec<u8> {
        cat(&[
            &LW_BYTES,
            &ZERO,
            &n(44),
            &n(6),
            &ZERO,
            &ZERO,
            &ZERO,
            &ZERO,
            &ZERO,
            &ZERO,
            &w("ABC"),
        ])
    }

    #[test]
    fn key_full_without_class() {
        let (r, b) = kq(KeyInfoClass::Full, &key(None), 44);
        assert_eq!(r, ok(44));
        assert_eq!(
            b,
            cat(&[&LW_BYTES, &ZERO, &NONE, &ZERO, &ZERO, &ZERO, &ZERO, &ZERO, &ZERO, &ZERO])
        );
    }

    #[test]
    fn key_full_overflow_copies_part_of_the_class() {
        let (r, b) = kq(KeyInfoClass::Full, &key(Some("ABC")), 47);
        assert_eq!(r, overflow(50));
        assert_eq!(b, full_plain()[..47]);
        let (r, b) = kq(KeyInfoClass::Full, &key(Some("ABC")), 44);
        assert_eq!(r, overflow(50));
        assert_eq!(b, full_plain()[..44]);
        let (r, b) = kq(KeyInfoClass::Full, &key(Some("ABC")), 50);
        assert_eq!(r, ok(50));
        assert_eq!(b, full_plain());
    }

    #[test]
    fn key_full_too_small() {
        for len in [0, 43] {
            let (r, b) = kq(KeyInfoClass::Full, &key(Some("ABC")), len);
            assert_eq!(r, too_small(50));
            assert_eq!(b, pad(len));
        }
    }

    // ---- KEY_NAME_INFORMATION: NameLength@0 Name@4 (the full NT path) ----

    #[test]
    fn key_name_exact_overflow_and_too_small() {
        let path = r"\REGISTRY\A";
        let full = cat(&[&n(22), &w(path)]);
        let q = |len| {
            run(len, |b| {
                write_key_info(KeyInfoClass::Name, &key(None), path, b)
            })
        };
        let (r, b) = q(26);
        assert_eq!(r, ok(26));
        assert_eq!(b, full);
        // WRK CmQueryKey copies as much of the name as fits.
        let (r, b) = q(10);
        assert_eq!(r, overflow(26));
        assert_eq!(b, full[..10]);
        let (r, b) = q(4);
        assert_eq!(r, overflow(26));
        assert_eq!(b, full[..4]);
        for len in [0, 3] {
            let (r, b) = q(len);
            assert_eq!(r, too_small(26));
            assert_eq!(b, pad(len));
        }
    }

    // ---- KEY_CACHED_INFORMATION (sizeof 40): LastWriteTime@0 TitleIndex@8 SubKeys@12
    //      MaxNameLen@16 Values@20 MaxValueNameLen@24 MaxValueDataLen@28 NameLength@32,
    //      36..40 padding. No name is copied; NameLength is the key's own (leaf) name. ----

    #[test]
    fn key_cached_exact() {
        let (r, b) = kq(KeyInfoClass::Cached, &merged(), 40);
        assert_eq!(r, ok(40));
        let expected = cat(&[
            &LW_BYTES,
            &ZERO,
            &n(2),
            &n(24),
            &n(2),
            &n(10),
            &n(10),
            &n(6), // "Mod"
            &pad(4),
        ]);
        assert_eq!(b, expected);
        // A larger buffer still reports and writes only the structure.
        let (r, b) = kq(KeyInfoClass::Cached, &merged(), 64);
        assert_eq!(r, ok(40));
        assert_eq!(b[..40], expected);
        assert_eq!(b[40..], pad(24));
    }

    #[test]
    fn key_cached_has_no_overflow_case() {
        // A fixed-size class: anything short of sizeof is too small, never a partial write.
        for len in [0, 36, 39] {
            let (r, b) = kq(KeyInfoClass::Cached, &merged(), len);
            assert_eq!(r, too_small(40));
            assert_eq!(b, pad(len));
        }
    }

    // ---- Fixed-size classes with nothing to report for an overlay key ----

    #[test]
    fn key_flags_virtualization_handle_tags() {
        for (class, size) in [
            (KeyInfoClass::Flags, 12),
            (KeyInfoClass::Virtualization, 4),
            (KeyInfoClass::HandleTags, 4),
        ] {
            let (r, b) = kq(class, &merged(), size);
            assert_eq!(r, ok(size as u32), "{class:?}");
            assert_eq!(b, vec![0; size], "{class:?}");
            for len in [0, size - 1] {
                let (r, b) = kq(class, &merged(), len);
                assert_eq!(r, too_small(size as u32), "{class:?}");
                assert_eq!(b, pad(len), "{class:?}");
            }
        }
    }

    // ---- NtEnumerateKey: a subkey by name with its own merged view ----

    #[test]
    fn subkey_basic_node_full() {
        let sub = key(Some("ABC"));
        let (r, b) = run(22, |b| {
            write_subkey_info(KeyInfoClass::Basic, "Mod", &sub, b)
        });
        assert_eq!(r, ok(22));
        assert_eq!(b, basic_mod());
        let (r, b) = run(38, |b| {
            write_subkey_info(KeyInfoClass::Node, "Mod", &sub, b)
        });
        assert_eq!(r, ok(36));
        assert_eq!(b, node_mod_abc());
        let (r, b) = run(50, |b| {
            write_subkey_info(KeyInfoClass::Full, "Mod", &sub, b)
        });
        assert_eq!(r, ok(50));
        assert_eq!(b, full_plain());
        // Short buffers behave as for NtQueryKey.
        let (r, b) = run(19, |b| {
            write_subkey_info(KeyInfoClass::Basic, "Mod", &sub, b)
        });
        assert_eq!(r, overflow(22));
        assert_eq!(b, basic_mod()[..19]);
        let (r, b) = run(15, |b| {
            write_subkey_info(KeyInfoClass::Basic, "Mod", &sub, b)
        });
        assert_eq!(r, too_small(22));
        assert_eq!(b, pad(15));
    }

    #[test]
    fn subkey_other_classes_are_invalid() {
        for class in [
            KeyInfoClass::Name,
            KeyInfoClass::Cached,
            KeyInfoClass::Flags,
            KeyInfoClass::Virtualization,
            KeyInfoClass::HandleTags,
        ] {
            let (r, b) = run(64, |b| write_subkey_info(class, "Mod", &merged(), b));
            assert_eq!(r.status, STATUS_INVALID_PARAMETER, "{class:?}");
            assert_eq!(b, pad(64));
        }
    }

    // ---- KEY_VALUE_BASIC_INFORMATION: TitleIndex@0 Type@4 NameLength@8 Name@12 ----

    #[test]
    fn value_basic_exact_overflow_too_small() {
        let v = val("Ab", 4, &[1, 2, 3, 4, 5]);
        let full = cat(&[&ZERO, &n(4), &n(4), &w("Ab")]);
        let (r, b) = vq(ValueInfoClass::Basic, &v, 16);
        assert_eq!(r, ok(16));
        assert_eq!(b, full);
        for len in [12, 14, 15] {
            let (r, b) = vq(ValueInfoClass::Basic, &v, len);
            assert_eq!(r, overflow(16));
            assert_eq!(b, full[..len]);
        }
        for len in [0, 11] {
            let (r, b) = vq(ValueInfoClass::Basic, &v, len);
            assert_eq!(r, too_small(16));
            assert_eq!(b, pad(len));
        }
    }

    // ---- KEY_VALUE_FULL_INFORMATION: TitleIndex@0 Type@4 DataOffset@8 DataLength@12
    //      NameLength@16 Name@20, data at ALIGN8(20 + NameLength) on x64 ----

    fn full_abc() -> Vec<u8> {
        // Name ends at 26; data at 32; 26..32 padding left alone.
        cat(&[
            &ZERO,
            &n(3),
            &n(32),
            &n(5),
            &n(6),
            &w("Abc"),
            &pad(6),
            &[1, 2, 3, 4, 5],
        ])
    }

    #[test]
    fn value_full_aligns_data_to_8() {
        let v = val("Abc", 3, &[1, 2, 3, 4, 5]);
        for class in [ValueInfoClass::Full, ValueInfoClass::FullAlign64] {
            let (r, b) = vq(class, &v, 37);
            assert_eq!(r, ok(37), "{class:?}");
            assert_eq!(b, full_abc(), "{class:?}");
        }
    }

    #[test]
    fn value_full_name_already_aligned() {
        let v = val("Ab", 4, &[1, 2, 3, 4, 5]);
        let expected = cat(&[
            &ZERO,
            &n(4),
            &n(24),
            &n(5),
            &n(4),
            &w("Ab"),
            &[1, 2, 3, 4, 5],
        ]);
        for class in [ValueInfoClass::Full, ValueInfoClass::FullAlign64] {
            let (r, b) = vq(class, &v, 29);
            assert_eq!(r, ok(29), "{class:?}");
            assert_eq!(b, expected, "{class:?}");
        }
    }

    #[test]
    fn value_full_without_data() {
        let v = val("Abc", 1, &[]);
        let expected = cat(&[&ZERO, &n(1), &NONE, &ZERO, &n(6), &w("Abc")]);
        for class in [ValueInfoClass::Full, ValueInfoClass::FullAlign64] {
            let (r, b) = vq(class, &v, 26);
            assert_eq!(r, ok(26), "{class:?}");
            assert_eq!(b, expected, "{class:?}");
        }
    }

    #[test]
    fn value_full_overflow_cases() {
        let v = val("Abc", 3, &[1, 2, 3, 4, 5]);
        for class in [ValueInfoClass::Full, ValueInfoClass::FullAlign64] {
            // Fixed part; part of the name; end inside the padding; part of the data.
            for len in [20, 23, 28, 34, 36] {
                let (r, b) = vq(class, &v, len);
                assert_eq!(r, overflow(37), "{class:?} {len}");
                assert_eq!(b, full_abc()[..len], "{class:?} {len}");
            }
            for len in [0, 19] {
                let (r, b) = vq(class, &v, len);
                assert_eq!(r, too_small(37), "{class:?}");
                assert_eq!(b, pad(len));
            }
        }
    }

    // ---- KEY_VALUE_PARTIAL_INFORMATION: TitleIndex@0 Type@4 DataLength@8 Data@12 ----

    #[test]
    fn value_partial_exact_overflow_too_small() {
        let v = val("ignored", 4, &[1, 2, 3, 4, 5]);
        let full = cat(&[&ZERO, &n(4), &n(5), &[1, 2, 3, 4, 5]]);
        let (r, b) = vq(ValueInfoClass::Partial, &v, 17);
        assert_eq!(r, ok(17));
        assert_eq!(b, full);
        // Review Focus 2: header fits, data does not.
        for len in [12, 14, 16] {
            let (r, b) = vq(ValueInfoClass::Partial, &v, len);
            assert_eq!(r, overflow(17));
            assert_eq!(b, full[..len]);
        }
        for len in [0, 1, 11] {
            let (r, b) = vq(ValueInfoClass::Partial, &v, len);
            assert_eq!(r, too_small(17));
            assert_eq!(b, pad(len));
        }
    }

    #[test]
    fn value_partial_without_data() {
        let v = val("x", 0xff00_ff00, &[]);
        let (r, b) = vq(ValueInfoClass::Partial, &v, 12);
        assert_eq!(r, ok(12));
        assert_eq!(b, cat(&[&ZERO, &n(0xff00_ff00), &ZERO]));
    }

    // ---- KEY_VALUE_PARTIAL_INFORMATION_ALIGN64: Type@0 DataLength@4 Data@8 ----

    #[test]
    fn value_partial_align64_exact_overflow_too_small() {
        let v = val("ignored", 4, &[1, 2, 3, 4, 5]);
        let full = cat(&[&n(4), &n(5), &[1, 2, 3, 4, 5]]);
        let (r, b) = vq(ValueInfoClass::PartialAlign64, &v, 13);
        assert_eq!(r, ok(13));
        assert_eq!(b, full);
        for len in [8, 10] {
            let (r, b) = vq(ValueInfoClass::PartialAlign64, &v, len);
            assert_eq!(r, overflow(13));
            assert_eq!(b, full[..len]);
        }
        for len in [0, 7] {
            let (r, b) = vq(ValueInfoClass::PartialAlign64, &v, len);
            assert_eq!(r, too_small(13));
            assert_eq!(b, pad(len));
        }
    }

    // ---- NtQueryMultipleValueKey ----

    const SEED: ValueEntry = ValueEntry {
        data_length: 0xAAAA,
        data_offset: 0xBBBB,
        ty: 0xCCCC,
    };

    #[test]
    fn multiple_all_fit_ulong_aligned() {
        let (a, b, c) = (
            val("a", 3, &[1, 2, 3]),
            val("b", 4, &[4, 5, 6, 7]),
            val("c", 1, &[8]),
        );
        let mut entries = [SEED; 3];
        let mut buf = vec![S; 9 + 4];
        let r = write_multiple_values(&[Some(&a), Some(&b), Some(&c)], &mut entries, &mut buf[..9]);
        assert_eq!(
            r,
            MultipleWritten {
                status: STATUS_SUCCESS,
                buffer_length: 9,
                result_length: 9
            }
        );
        assert_eq!(buf, [1, 2, 3, S, 4, 5, 6, 7, 8, S, S, S, S]);
        let e = |data_length, data_offset, ty| ValueEntry {
            data_length,
            data_offset,
            ty,
        };
        assert_eq!(entries, [e(3, 0, 3), e(4, 4, 4), e(1, 8, 1)]);
    }

    #[test]
    fn multiple_overflow_stops_filling_at_the_first_value_that_does_not_fit() {
        let (a, b, c) = (
            val("a", 3, &[1, 2, 3]),
            val("b", 4, &[0x55; 10]),
            val("c", 1, &[8]),
        );
        let mut entries = [SEED; 3];
        let mut buf = vec![S; 6];
        let r = write_multiple_values(&[Some(&a), Some(&b), Some(&c)], &mut entries, &mut buf);
        // c would fit at offset 4, but WRK stops copying once the buffer is full.
        // BufferLength is the used length, rounded up before the value that did not fit.
        assert_eq!(
            r,
            MultipleWritten {
                status: STATUS_BUFFER_OVERFLOW,
                buffer_length: 4,
                result_length: 17
            }
        );
        assert_eq!(buf, [1, 2, 3, S, S, S]);
        assert_eq!(
            entries,
            [
                ValueEntry {
                    data_length: 3,
                    data_offset: 0,
                    ty: 3
                },
                SEED,
                SEED
            ]
        );
    }

    #[test]
    fn multiple_missing_value_is_name_not_found() {
        let (a, c) = (val("a", 3, &[1, 2, 3]), val("c", 1, &[8]));
        let mut entries = [SEED; 3];
        let mut buf = vec![S; 16];
        let r = write_multiple_values(&[Some(&a), None, Some(&c)], &mut entries, &mut buf);
        assert_eq!(r.status, STATUS_OBJECT_NAME_NOT_FOUND);
        assert_eq!(buf[..4], [1, 2, 3, S]);
        assert_eq!(buf[4..], pad(12));
        assert_eq!(entries[1..], [SEED, SEED]);
        assert_eq!(entries[0].data_length, 3);
    }

    #[test]
    fn multiple_empty_data_into_empty_buffer() {
        let a = val("a", 7, &[]);
        let mut entries = [SEED; 1];
        let r = write_multiple_values(&[Some(&a)], &mut entries, &mut []);
        assert_eq!(
            r,
            MultipleWritten {
                status: STATUS_SUCCESS,
                buffer_length: 0,
                result_length: 0
            }
        );
        assert_eq!(
            entries[0],
            ValueEntry {
                data_length: 0,
                data_offset: 0,
                ty: 7
            }
        );
    }

    #[test]
    fn value_entry_store_leaves_the_name_pointer() {
        // KEY_VALUE_ENTRY (x64): ValueName@0 (8) DataLength@8 DataOffset@12 Type@16, size 24.
        let mut slot = [S; KEY_VALUE_ENTRY_SIZE];
        ValueEntry {
            data_length: 0x0102_0304,
            data_offset: 0x10,
            ty: 4,
        }
        .store(&mut slot);
        let expected = cat(&[&pad(8), &n(0x0102_0304), &n(0x10), &n(4), &pad(4)]);
        assert_eq!(slot[..], expected[..]);
    }
}
