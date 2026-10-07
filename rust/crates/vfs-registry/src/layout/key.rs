//! Key information classes: `NtQueryKey` and `NtEnumerateKey`.
use super::*;
use crate::merge::KeyView;
use crate::path::leaf;

fn class_bytes(k: &KeyView) -> Vec<u8> {
    k.class
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .flat_map(|u| u.to_le_bytes())
        .collect()
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

fn counts(k: &KeyView) -> Counts {
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

/// KEY_BASIC_INFORMATION: LastWriteTime@0 TitleIndex@8 NameLength@12 Name@16.
fn key_basic(name: &str, k: &KeyView, buf: &mut [u8]) -> Written {
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
fn key_node(name: &str, k: &KeyView, buf: &mut [u8]) -> Written {
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
fn key_full(k: &KeyView, buf: &mut [u8]) -> Written {
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
fn key_cached(path: &str, k: &KeyView, buf: &mut [u8]) -> Written {
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
    key: &KeyView,
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
    sub: &KeyView,
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
