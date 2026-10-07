use super::*;
use crate::merge::{MergedKey, RealKey, merge};
use crate::overlay::{Overlay, Value};

mod key;
mod value;

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
