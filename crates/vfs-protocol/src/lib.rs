//! Pure little-endian FUSE-like wire codecs for the director control ring.
//! No OS I/O and no `vfs-core` — only encode/decode + status/opcode constants.
#![forbid(unsafe_code)]

// Opcode catalog — must match `vfs_ipc::layout` values (do not renumber).
pub const OP_GETATTR: u32 = 1;
pub const OP_READDIR: u32 = 2;
pub const OP_OPEN: u32 = 3;
pub const OP_MATERIALIZE: u32 = 4;
pub const OP_READ: u32 = 5;
pub const OP_WRITE: u32 = 6;
pub const OP_SETATTR: u32 = 7;
pub const OP_RENAME: u32 = 8;
pub const OP_DELETE: u32 = 9;
pub const OP_MKDIR: u32 = 10;
pub const OP_CLOSE: u32 = 11;
pub const OP_REGISTER_PROCESS: u32 = 12;
pub const OP_HEARTBEAT: u32 = 13;

pub const ST_OK: i32 = 0;
pub const ST_NOT_FOUND: i32 = -1;
pub const ST_NOT_A_DIRECTORY: i32 = -2;
pub const ST_BAD_REQUEST: i32 = -3;
pub const ST_IO_ERROR: i32 = -4;
pub const ST_IS_DIR: i32 = -5;
pub const ST_BAD_FH: i32 = -6;
pub const ST_NO_SPACE: i32 = -7;

/// OPEN request wants read access.
pub const OPEN_READ: u32 = 1;
/// OPEN request wants write access (unsupported in MVP director → BAD_REQUEST).
pub const OPEN_WRITE: u32 = 2;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenResp {
    pub fh: u64,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadReq {
    pub fh: u64,
    pub offset: u64,
    pub len: u32,
}

pub fn encode_path_req(vpath: &str) -> Vec<u8> {
    vpath.as_bytes().to_vec()
}

pub fn decode_path_req(payload: &[u8]) -> Option<String> {
    core::str::from_utf8(payload).ok().map(|s| s.to_string())
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
    if p.len() < 18 {
        return None;
    }
    let size = u64::from_le_bytes(p[2..10].try_into().ok()?);
    let mtime = i64::from_le_bytes(p[10..18].try_into().ok()?);
    Some(AttrResp {
        found: p[0] != 0,
        is_dir: p[1] != 0,
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
    let mut off = 0usize;
    let count = take_u32(p, &mut off)?;
    let mut out = Vec::new();
    for _ in 0..count {
        let nlen = take_u32(p, &mut off)? as usize;
        let end = off.checked_add(nlen)?;
        let name = core::str::from_utf8(p.get(off..end)?).ok()?.to_string();
        off = end;
        let is_dir = take_u8(p, &mut off)? != 0;
        let size = take_u64(p, &mut off)?;
        let mtime = take_u64(p, &mut off)? as i64;
        out.push(DirEntryWire {
            name,
            is_dir,
            size,
            mtime,
        });
    }
    Some(out)
}

/// OPEN req: `flags:u32 | path_utf8…`
pub fn encode_open_req(flags: u32, path: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(4 + path.len());
    b.extend_from_slice(&flags.to_le_bytes());
    b.extend_from_slice(path.as_bytes());
    b
}

pub fn decode_open_req(p: &[u8]) -> Option<(u32, String)> {
    if p.len() < 4 {
        return None;
    }
    let flags = u32::from_le_bytes(p[0..4].try_into().ok()?);
    let path = core::str::from_utf8(&p[4..]).ok()?.to_string();
    Some((flags, path))
}

/// OPEN resp: `fh:u64 | size:u64 | is_dir:u8 | pad[7]`
pub fn encode_open_resp(r: &OpenResp) -> Vec<u8> {
    let mut b = Vec::with_capacity(24);
    b.extend_from_slice(&r.fh.to_le_bytes());
    b.extend_from_slice(&r.size.to_le_bytes());
    b.push(r.is_dir as u8);
    b.extend_from_slice(&[0u8; 7]);
    b
}

pub fn decode_open_resp(p: &[u8]) -> Option<OpenResp> {
    if p.len() < 24 {
        return None;
    }
    let fh = u64::from_le_bytes(p[0..8].try_into().ok()?);
    let size = u64::from_le_bytes(p[8..16].try_into().ok()?);
    let is_dir = p[16] != 0;
    Some(OpenResp { fh, size, is_dir })
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
    if p.len() < 20 {
        return None;
    }
    let fh = u64::from_le_bytes(p[0..8].try_into().ok()?);
    let offset = u64::from_le_bytes(p[8..16].try_into().ok()?);
    let len = u32::from_le_bytes(p[16..20].try_into().ok()?);
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
    if p.len() < 8 {
        return None;
    }
    let n = u32::from_le_bytes(p[0..4].try_into().ok()?) as usize;
    if p.len() < 8 + n {
        return None;
    }
    Some(p[8..8 + n].to_vec())
}

pub fn encode_close_req(fh: u64) -> Vec<u8> {
    fh.to_le_bytes().to_vec()
}

pub fn decode_close_req(p: &[u8]) -> Option<u64> {
    if p.len() < 8 {
        return None;
    }
    Some(u64::from_le_bytes(p[0..8].try_into().ok()?))
}

fn take_u32(p: &[u8], off: &mut usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    let s = p.get(*off..end)?;
    *off = end;
    Some(u32::from_le_bytes(s.try_into().ok()?))
}
fn take_u64(p: &[u8], off: &mut usize) -> Option<u64> {
    let end = off.checked_add(8)?;
    let s = p.get(*off..end)?;
    *off = end;
    Some(u64::from_le_bytes(s.try_into().ok()?))
}
fn take_u8(p: &[u8], off: &mut usize) -> Option<u8> {
    let v = *p.get(*off)?;
    *off += 1;
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_req_roundtrip() {
        let p = encode_open_req(OPEN_READ, "Data/Skyrim.esm");
        let (f, path) = decode_open_req(&p).unwrap();
        assert_eq!(f, OPEN_READ);
        assert_eq!(path, "Data/Skyrim.esm");
    }

    #[test]
    fn open_resp_roundtrip() {
        let r = OpenResp {
            fh: 42,
            size: 1000,
            is_dir: false,
        };
        assert_eq!(decode_open_resp(&encode_open_resp(&r)), Some(r));
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
    }

    #[test]
    fn close_req_roundtrip() {
        assert_eq!(decode_close_req(&encode_close_req(99)), Some(99));
    }

    #[test]
    fn opcode_constants_match_ipc_catalog() {
        assert_eq!(OP_OPEN, 3);
        assert_eq!(OP_READ, 5);
        assert_eq!(OP_CLOSE, 11);
        assert_eq!(OP_GETATTR, 1);
        assert_eq!(OP_READDIR, 2);
        assert_eq!(OP_HEARTBEAT, 13);
    }

    #[test]
    fn short_buffers_decode_none() {
        assert!(decode_open_req(&[1, 2]).is_none());
        assert!(decode_read_req(&[0u8; 10]).is_none());
        assert!(decode_read_resp(&[1, 0, 0]).is_none());
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
}
