//! Protocol payload encoding for the message catalog.

pub const ST_OK: i32 = 0;
pub const ST_NOT_FOUND: i32 = -1;
pub const ST_NOT_A_DIRECTORY: i32 = -2;
pub const ST_BAD_REQUEST: i32 = -3;

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
    Some(AttrResp { found: p[0] != 0, is_dir: p[1] != 0, size, mtime })
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

pub fn decode_readdir_resp(p: &[u8]) -> Option<Vec<DirEntryWire>> {
    let mut off = 0usize;
    let count = take_u32(p, &mut off)?;
    // Do NOT pre-allocate from an untrusted count.
    let mut out = Vec::new();
    for _ in 0..count {
        let nlen = take_u32(p, &mut off)? as usize;
        let end = off.checked_add(nlen)?;
        let name = core::str::from_utf8(p.get(off..end)?).ok()?.to_string();
        off = end;
        let is_dir = take_u8(p, &mut off)? != 0;
        let size = take_u64(p, &mut off)?;
        let mtime = take_u64(p, &mut off)? as i64;
        out.push(DirEntryWire { name, is_dir, size, mtime });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn getattr_resp_roundtrip() {
        let r = AttrResp { found: true, is_dir: false, size: 123, mtime: -7 };
        assert_eq!(decode_getattr_resp(&encode_getattr_resp(&r)), Some(r));
        let nf = AttrResp { found: false, is_dir: false, size: 0, mtime: 0 };
        assert_eq!(decode_getattr_resp(&encode_getattr_resp(&nf)), Some(nf));
    }

    #[test]
    fn getattr_resp_short_is_none() {
        assert_eq!(decode_getattr_resp(&[1, 0, 0]), None);
        assert_eq!(decode_getattr_resp(&[]), None);
    }

    #[test]
    fn readdir_resp_roundtrip() {
        let entries = vec![
            DirEntryWire { name: "a.esp".into(), is_dir: false, size: 10, mtime: 1 },
            DirEntryWire { name: "sub".into(), is_dir: true, size: 0, mtime: 0 },
        ];
        assert_eq!(decode_readdir_resp(&encode_readdir_resp(&entries)), Some(entries));
    }

    #[test]
    fn empty_readdir_roundtrips() {
        assert_eq!(decode_readdir_resp(&encode_readdir_resp(&[])), Some(vec![]));
    }

    #[test]
    fn readdir_resp_truncated_is_none() {
        let entries = vec![DirEntryWire { name: "abc".into(), is_dir: false, size: 5, mtime: 2 }];
        let mut enc = encode_readdir_resp(&entries);
        enc.truncate(enc.len() - 3);
        assert_eq!(decode_readdir_resp(&enc), None);
    }

    #[test]
    fn path_req_roundtrip() {
        assert_eq!(decode_path_req(&encode_path_req("data/a.esp")), Some("data/a.esp".to_string()));
        // invalid UTF-8 → None
        assert_eq!(decode_path_req(&[0xFF, 0xFE]), None);
    }
}
