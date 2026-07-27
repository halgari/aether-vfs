//! Snapshot byte layout: structs, offsets, LE field helpers.

use core::mem::{align_of, offset_of, size_of};

pub const MAGIC: u32 = 0x5646_5353;
pub const VERSION: u32 = 1;
pub const KIND_DIR: u8 = 0;
pub const KIND_FILE: u8 = 1;
pub const KIND_TOMBSTONE: u8 = 2;

#[repr(C)]
pub struct Header {
    pub magic: u32,
    pub version: u32,
    pub generation: u64,
    pub total_len: u32,
    pub root_node: u32,
    pub node_count: u32,
    pub nodes_off: u32,
    pub child_count: u32,
    pub children_off: u32,
    pub strings_len: u32,
    pub strings_off: u32,
}

#[repr(C)]
pub struct SnapNode {
    pub kind: u8,
    pub _pad0: [u8; 3],
    pub layer: u32,
    pub name_off: u32,
    pub name_len: u32,
    pub child_first: u32,
    pub child_count: u32,
    pub source_off: u32,
    pub source_len: u32,
    pub size: u64,
    pub mtime: i64,
    pub cache_key: [u8; 32],
}

#[repr(C)]
pub struct SnapChild {
    pub folded_off: u32,
    pub folded_len: u32,
    pub node: u32,
    pub _pad: u32,
}

pub const HEADER_SIZE: usize = size_of::<Header>();
pub const NODE_SIZE: usize = size_of::<SnapNode>();
pub const CHILD_SIZE: usize = size_of::<SnapChild>();

const _: () = assert!(HEADER_SIZE == 48 && align_of::<Header>() == 8);
const _: () = assert!(NODE_SIZE == 80 && align_of::<SnapNode>() == 8);
const _: () = assert!(CHILD_SIZE == 16);

pub const H_MAGIC: usize = offset_of!(Header, magic);
pub const H_VERSION: usize = offset_of!(Header, version);
pub const H_GENERATION: usize = offset_of!(Header, generation);
pub const H_TOTAL_LEN: usize = offset_of!(Header, total_len);
pub const H_ROOT_NODE: usize = offset_of!(Header, root_node);
pub const H_NODE_COUNT: usize = offset_of!(Header, node_count);
pub const H_NODES_OFF: usize = offset_of!(Header, nodes_off);
pub const H_CHILD_COUNT: usize = offset_of!(Header, child_count);
pub const H_CHILDREN_OFF: usize = offset_of!(Header, children_off);
pub const H_STRINGS_LEN: usize = offset_of!(Header, strings_len);
pub const H_STRINGS_OFF: usize = offset_of!(Header, strings_off);

pub const N_KIND: usize = offset_of!(SnapNode, kind);
pub const N_LAYER: usize = offset_of!(SnapNode, layer);
pub const N_NAME_OFF: usize = offset_of!(SnapNode, name_off);
pub const N_NAME_LEN: usize = offset_of!(SnapNode, name_len);
pub const N_CHILD_FIRST: usize = offset_of!(SnapNode, child_first);
pub const N_CHILD_COUNT: usize = offset_of!(SnapNode, child_count);
pub const N_SOURCE_OFF: usize = offset_of!(SnapNode, source_off);
pub const N_SOURCE_LEN: usize = offset_of!(SnapNode, source_len);
pub const N_SIZE: usize = offset_of!(SnapNode, size);
pub const N_MTIME: usize = offset_of!(SnapNode, mtime);
pub const N_CACHE_KEY: usize = offset_of!(SnapNode, cache_key);

pub const C_FOLDED_OFF: usize = offset_of!(SnapChild, folded_off);
pub const C_FOLDED_LEN: usize = offset_of!(SnapChild, folded_len);
pub const C_NODE: usize = offset_of!(SnapChild, node);

pub fn read_u8(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}
pub fn read_u32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes(s.try_into().ok()?))
}
pub fn read_u64(b: &[u8], off: usize) -> Option<u64> {
    let s = b.get(off..off.checked_add(8)?)?;
    Some(u64::from_le_bytes(s.try_into().ok()?))
}
pub fn read_i64(b: &[u8], off: usize) -> Option<i64> {
    let s = b.get(off..off.checked_add(8)?)?;
    Some(i64::from_le_bytes(s.try_into().ok()?))
}
pub fn read_key(b: &[u8], off: usize) -> Option<[u8; 32]> {
    let s = b.get(off..off.checked_add(32)?)?;
    Some(s.try_into().ok()?)
}
pub fn read_slice(b: &[u8], off: usize, len: usize) -> Option<&[u8]> {
    b.get(off..off.checked_add(len)?)
}

pub fn write_u8(b: &mut [u8], off: usize, v: u8) {
    b[off] = v;
}
pub fn write_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
pub fn write_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
pub fn write_i64(b: &mut [u8], off: usize, v: i64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
pub fn write_key(b: &mut [u8], off: usize, v: &[u8; 32]) {
    b[off..off + 32].copy_from_slice(v);
}
pub fn write_bytes(b: &mut [u8], off: usize, v: &[u8]) {
    b[off..off + v.len()].copy_from_slice(v);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_offsets_are_expected() {
        assert_eq!(H_MAGIC, 0);
        assert_eq!(H_VERSION, 4);
        assert_eq!(H_GENERATION, 8);
        assert_eq!(H_TOTAL_LEN, 16);
        assert_eq!(H_STRINGS_OFF, 44);
        assert_eq!(HEADER_SIZE, 48);
    }

    #[test]
    fn node_offsets_are_expected() {
        assert_eq!(N_KIND, 0);
        assert_eq!(N_LAYER, 4);
        assert_eq!(N_SIZE, 32);
        assert_eq!(N_MTIME, 40);
        assert_eq!(N_CACHE_KEY, 48);
        assert_eq!(NODE_SIZE, 80);
    }

    #[test]
    fn le_roundtrip() {
        let mut b = vec![0u8; 64];
        write_u32(&mut b, 4, 0xDEAD_BEEF);
        write_u64(&mut b, 8, 0x0102_0304_0506_0708);
        write_i64(&mut b, 16, -42);
        assert_eq!(read_u32(&b, 4), Some(0xDEAD_BEEF));
        assert_eq!(read_u64(&b, 8), Some(0x0102_0304_0506_0708));
        assert_eq!(read_i64(&b, 16), Some(-42));
    }

    #[test]
    fn reads_out_of_bounds_return_none() {
        let b = vec![0u8; 4];
        assert_eq!(read_u32(&b, 2), None);
        assert_eq!(read_u64(&b, 0), None);
        assert_eq!(read_slice(&b, 3, 4), None);
        assert_eq!(read_u8(&b, 4), None);
    }
}
