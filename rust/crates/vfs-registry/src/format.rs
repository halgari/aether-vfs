//! Persistence format for the overlay.
//!
//! Little-endian and length-prefixed: `MAGIC`, a presence byte, one depth-first node tree rooted
//! at `\Registry`, then a checksum (wrapping sum of the preceding bytes as zero-padded u64
//! words). A node is: stored spelling of its key name (the root stores `Registry`), flags (bit 0
//! created), last-write FILETIME, then the counts of values, value tombstones, child tombstones
//! and child nodes, then those items in that order (a value is name, type u32, data), the
//! children following inline. Volatile nodes and their subtrees are not written.
//!
//! `decode` validates everything and never panics: a damaged file is a [`FormatError`].
use crate::overlay::{
    Child, Node, Overlay, Value, MAX_DATA, MAX_DECODE_DEPTH, MAX_KEY_NAME, MAX_VALUE_NAME,
};
use crate::path::fold;
use std::collections::BTreeSet;

pub const MAGIC: &[u8; 8] = b"AEREG\0\0\x01";

/// Deepest key nesting `decode` accepts (bounds recursion). Above the 512 write cap so files
/// from the old, uncapped writer still load.
const MAX_DEPTH: usize = MAX_DECODE_DEPTH;
const FLAG_CREATED: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    Truncated,
    BadMagic,
    BadChecksum,
    /// The bytes parse but describe an impossible overlay.
    Invalid(&'static str),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::Truncated => f.write_str("registry overlay file is truncated"),
            FormatError::BadMagic => f.write_str("not a registry overlay file of this version"),
            FormatError::BadChecksum => f.write_str("registry overlay file is corrupt"),
            FormatError::Invalid(why) => write!(f, "registry overlay file is invalid: {why}"),
        }
    }
}

impl std::error::Error for FormatError {}

fn checksum(b: &[u8]) -> u64 {
    let mut sum = 0u64;
    for c in b.chunks(8) {
        let mut w = [0u8; 8];
        w[..c.len()].copy_from_slice(c);
        sum = sum.wrapping_add(u64::from_le_bytes(w));
    }
    sum
}

fn put_u32(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&(n as u32).to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len());
    out.extend_from_slice(s.as_bytes());
}

fn encode_node(o: &Overlay, spelling: &str, path: &str, node: &Node, out: &mut Vec<u8>) {
    put_str(out, spelling);
    out.push(if node.created { FLAG_CREATED } else { 0 });
    out.extend_from_slice(&node.last_write.to_le_bytes());
    let mut tombs = Vec::new();
    let mut kids = Vec::new();
    for (name, st) in node.children.values() {
        match st {
            Child::Tombstone => tombs.push(name.as_str()),
            Child::Present => {
                let p = format!("{path}\\{name}");
                if let Some(n) = o.node(&p).filter(|n| !n.volatile) {
                    kids.push((name.as_str(), p, n));
                }
            }
        }
    }
    put_u32(out, node.values.len());
    put_u32(out, node.value_tombstones.len());
    put_u32(out, tombs.len());
    put_u32(out, kids.len());
    for v in &node.values {
        put_str(out, &v.name);
        out.extend_from_slice(&v.ty.to_le_bytes());
        put_u32(out, v.data.len());
        out.extend_from_slice(&v.data);
    }
    for t in &node.value_tombstones {
        put_str(out, t);
    }
    for t in tombs {
        put_str(out, t);
    }
    for (name, p, n) in kids {
        encode_node(o, name, &p, n, out);
    }
}

/// Serialise the overlay; volatile nodes and their subtrees are left out.
pub fn encode(o: &Overlay) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    let mut root = None;
    o.walk(|p, n| {
        if root.is_none() {
            root = Some((p.to_string(), n.clone()));
        }
    });
    match root {
        Some((p, n)) if !n.volatile && p.matches('\\').count() == 1 => {
            out.push(1);
            encode_node(o, &p[1..], &p, &n, &mut out);
        }
        _ => out.push(0),
    }
    let sum = checksum(&out);
    out.extend_from_slice(&sum.to_le_bytes());
    out
}

struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        if n > self.b.len() {
            return Err(FormatError::Truncated);
        }
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Ok(h)
    }

    fn u8(&mut self) -> Result<u8, FormatError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, FormatError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, FormatError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    /// An item count; every item takes at least one byte, so a count beyond the remaining
    /// bytes is impossible (and must not drive an allocation).
    fn count(&mut self) -> Result<usize, FormatError> {
        let n = self.u32()? as usize;
        if n > self.b.len() {
            return Err(FormatError::Truncated);
        }
        Ok(n)
    }

    fn string(&mut self, max_utf16: usize) -> Result<String, FormatError> {
        let n = self.u32()? as usize;
        // UTF-8 is at most three bytes per UTF-16 unit.
        if n > max_utf16.saturating_mul(3) {
            return Err(FormatError::Invalid("name too long"));
        }
        let s = std::str::from_utf8(self.take(n)?)
            .map_err(|_| FormatError::Invalid("invalid UTF-8"))?;
        if s.encode_utf16().count() > max_utf16 {
            return Err(FormatError::Invalid("name too long"));
        }
        Ok(s.to_string())
    }

    fn key_name(&mut self) -> Result<String, FormatError> {
        let s = self.string(MAX_KEY_NAME)?;
        if s.is_empty() || s.contains('\\') {
            return Err(FormatError::Invalid("bad key name"));
        }
        Ok(s)
    }
}

fn decode_node(
    r: &mut Reader,
    path: String,
    depth: usize,
    out: &mut Vec<(String, Node)>,
) -> Result<(), FormatError> {
    if depth > MAX_DEPTH {
        return Err(FormatError::Invalid("keys nested too deeply"));
    }
    let flags = r.u8()?;
    if flags & !FLAG_CREATED != 0 {
        return Err(FormatError::Invalid("unknown or volatile node flags"));
    }
    let mut node = Node {
        created: flags & FLAG_CREATED != 0,
        last_write: r.u64()?,
        ..Node::default()
    };
    let (nv, nt, nct, nc) = (r.count()?, r.count()?, r.count()?, r.count()?);
    let mut seen = BTreeSet::new();
    for _ in 0..nv {
        let name = r.string(MAX_VALUE_NAME)?;
        let ty = r.u32()?;
        let len = r.u32()? as usize;
        if len > MAX_DATA {
            return Err(FormatError::Invalid("value data too large"));
        }
        let data = r.take(len)?.to_vec();
        if !seen.insert(fold(&name)) {
            return Err(FormatError::Invalid("duplicate value"));
        }
        node.values.push(Value { name, ty, data });
    }
    for _ in 0..nt {
        let name = r.string(MAX_VALUE_NAME)?;
        if fold(&name) != name {
            return Err(FormatError::Invalid("value tombstone not folded"));
        }
        if !seen.insert(name.clone()) {
            return Err(FormatError::Invalid("value both present and tombstoned"));
        }
        node.value_tombstones.push(name);
    }
    for _ in 0..nct {
        let name = r.key_name()?;
        if node
            .children
            .insert(fold(&name), (name, Child::Tombstone))
            .is_some()
        {
            return Err(FormatError::Invalid("duplicate child"));
        }
    }
    let slot = out.len();
    out.push((path.clone(), Node::default()));
    for _ in 0..nc {
        let name = r.key_name()?;
        let folded = fold(&name);
        if node
            .children
            .insert(folded, (name.clone(), Child::Present))
            .is_some()
        {
            return Err(FormatError::Invalid("duplicate child"));
        }
        decode_node(r, format!("{path}\\{name}"), depth + 1, out)?;
    }
    out[slot].1 = node;
    Ok(())
}

/// Parse and fully validate a saved overlay. The version restarts at 1.
pub fn decode(b: &[u8]) -> Result<Overlay, FormatError> {
    if b.len() < MAGIC.len() || b[..8] != MAGIC[..] {
        return Err(if b.len() < MAGIC.len() {
            FormatError::Truncated
        } else {
            FormatError::BadMagic
        });
    }
    if b.len() < MAGIC.len() + 1 + 8 {
        return Err(FormatError::Truncated);
    }
    let (body, sum) = b.split_at(b.len() - 8);
    if checksum(body) != u64::from_le_bytes(sum.try_into().unwrap()) {
        return Err(FormatError::BadChecksum);
    }
    let mut r = Reader {
        b: &body[MAGIC.len()..],
    };
    let mut nodes = Vec::new();
    match r.u8()? {
        0 => {}
        1 => {
            let name = r.key_name()?;
            if fold(&name) != "registry" {
                return Err(FormatError::Invalid("root is not Registry"));
            }
            decode_node(&mut r, format!("\\{name}"), 0, &mut nodes)?;
        }
        _ => return Err(FormatError::Invalid("bad presence byte")),
    }
    if !r.b.is_empty() {
        return Err(FormatError::Invalid("trailing bytes"));
    }
    let mut o = Overlay::new();
    for (p, n) in nodes {
        o.insert_node(&p, n, 1)
            .map_err(|_| FormatError::Invalid("bad key path"))?;
    }
    o.set_version(1);
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::{Child, Lookup, Node, Value};

    const K: &str = r"\Registry\Machine\Software\Mod";

    fn sample() -> Overlay {
        let mut o = Overlay::new();
        o.set_value(K, "str", 1, b"h\0i\0\0\0", 10).unwrap();
        o.set_value(K, "Bin", 3, &[0, 1, 2, 255], 11).unwrap();
        o.set_value(K, "dw", 4, &7u32.to_le_bytes(), 12).unwrap();
        o.set_value(K, "multi", 7, b"a\0\0\0\0\0", 13).unwrap();
        o.set_value(K, "qw", 11, &9u64.to_le_bytes(), 14).unwrap();
        o.set_value(K, "empty", 0, b"", 15).unwrap();
        o.set_value(K, "gone", 1, b"x", 16).unwrap();
        o.delete_value(K, "Gone", 17).unwrap();
        o.delete_value(K, "ghost", 18).unwrap();
        o.create_key(&format!(r"{K}\Sub\Deep"), false, false, 19)
            .unwrap();
        o.set_value(&format!(r"{K}\Sub"), "é\u{1F600}", 1, b"u", 20)
            .unwrap();
        o.delete_key(&format!(r"{K}\Dead"), 21).unwrap();
        o
    }

    fn dump(o: &Overlay) -> Vec<(String, Node)> {
        let mut v = Vec::new();
        o.walk(|p, n| v.push((p.to_string(), n.clone())));
        v
    }

    /// Recompute the trailing checksum after editing the body.
    fn reseal(mut b: Vec<u8>) -> Vec<u8> {
        b.truncate(b.len() - 8);
        let s = checksum(&b);
        b.extend_from_slice(&s.to_le_bytes());
        b
    }

    #[test]
    fn round_trip_all_types_and_tombstones() {
        let o = sample();
        let bytes = encode(&o);
        assert_eq!(&bytes[..8], MAGIC);
        let r = decode(&bytes).unwrap();
        assert_eq!(dump(&r), dump(&o));
        assert_eq!(r.version(), 1);
        assert_eq!(r.lookup(&format!(r"{K}\dead")).0, Lookup::Tombstoned);
        assert_eq!(r.node(K).unwrap().value_tombstones.len(), 2);
        assert_eq!(encode(&r), bytes);
    }

    #[test]
    fn empty_overlay_round_trips() {
        let o = Overlay::new();
        let r = decode(&encode(&o)).unwrap();
        assert!(dump(&r).is_empty());
        assert_eq!(r.version(), 1);
    }

    #[test]
    fn volatile_nodes_and_subtrees_are_not_saved() {
        let mut o = Overlay::new();
        o.create_key(&format!(r"{K}\Keep"), false, false, 1)
            .unwrap();
        o.create_key(&format!(r"{K}\Temp"), true, false, 2).unwrap();
        o.create_key(&format!(r"{K}\Temp\Child"), false, false, 3)
            .unwrap();
        let r = decode(&encode(&o)).unwrap();
        assert!(r.node(&format!(r"{K}\Keep")).is_some());
        assert!(r.node(&format!(r"{K}\Temp")).is_none());
        assert!(r.node(&format!(r"{K}\Temp\Child")).is_none());
        assert!(!r.node(K).unwrap().children.contains_key("temp"));
        assert_eq!(
            r.node(K).unwrap().children.get("keep").map(|c| c.1),
            Some(Child::Present)
        );
    }

    #[test]
    fn truncated_or_corrupt_is_an_error() {
        let bytes = encode(&sample());
        assert!(decode(&[]).is_err());
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
        let mut bad = bytes.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0x40;
        assert_eq!(decode(&bad).unwrap_err(), FormatError::BadChecksum);
        let mut bad = bytes.clone();
        bad.extend_from_slice(&[0; 8]);
        assert!(decode(&bad).is_err());
    }

    #[test]
    fn version_mismatch_is_refused() {
        let mut bytes = encode(&sample());
        bytes[7] = 2;
        assert_eq!(decode(&bytes).unwrap_err(), FormatError::BadMagic);
        let mut bytes = encode(&sample());
        bytes[0] = b'X';
        assert_eq!(decode(&bytes).unwrap_err(), FormatError::BadMagic);
    }

    #[test]
    fn decode_never_panics_on_truncations_and_mutations() {
        let bytes = encode(&sample());
        for n in 0..=bytes.len() {
            let _ = decode(&bytes[..n]);
            // Also with a valid checksum, so the parser itself sees the cut body.
            if n >= 16 {
                let _ = decode(&reseal(bytes[..n].iter().copied().chain([0; 8]).collect()));
            }
        }
        // Every single-byte mutation, with and without a repaired checksum.
        for i in 0..bytes.len() - 8 {
            for x in [0x00u8, 0x01, 0x5c, 0x7f, 0x80, 0xff] {
                let mut m = bytes.clone();
                m[i] = x;
                let _ = decode(&m);
                let _ = decode(&reseal(m));
            }
        }
    }

    fn seal(body: &[u8]) -> Vec<u8> {
        let mut b = MAGIC.to_vec();
        b.extend_from_slice(body);
        b.extend_from_slice(&[0; 8]);
        reseal(b)
    }
    fn s(x: &str) -> Vec<u8> {
        let mut v = (x.len() as u32).to_le_bytes().to_vec();
        v.extend_from_slice(x.as_bytes());
        v
    }
    // node: spelling, flags, last_write, nvalues, ntomb, nctomb, nchildren
    fn node(name: &[u8], flags: u8, nv: u32, nt: u32, nct: u32, nc: u32) -> Vec<u8> {
        let mut v = name.to_vec();
        v.push(flags);
        v.extend_from_slice(&0u64.to_le_bytes());
        for n in [nv, nt, nct, nc] {
            v.extend_from_slice(&n.to_le_bytes());
        }
        v
    }

    #[test]
    fn inconsistent_bodies_are_refused() {
        // Hand-build bodies behind a valid checksum.
        let good = {
            let mut b = vec![1u8];
            b.extend(node(&s("Registry"), 0, 0, 0, 0, 0));
            b
        };
        assert!(decode(&seal(&good)).is_ok());
        // Absurd counts.
        let mut b = vec![1u8];
        b.extend(node(&s("Registry"), 0, u32::MAX, 0, 0, 0));
        assert!(decode(&seal(&b)).is_err());
        let mut b = vec![1u8];
        b.extend(node(&s("Registry"), 0, 0, 0, 0, u32::MAX));
        assert!(decode(&seal(&b)).is_err());
        // Wrong root, empty name, separator in name, bad UTF-8, volatile flag, unknown flag.
        for name in [s("Other"), s(""), s("Reg\\x")] {
            let mut b = vec![1u8];
            b.extend(node(&name, 0, 0, 0, 0, 0));
            assert!(decode(&seal(&b)).is_err());
        }
        let mut b = vec![1u8];
        b.extend(node(&[2, 0, 0, 0, 0xff, 0xfe], 0, 0, 0, 0, 0));
        assert!(decode(&seal(&b)).is_err());
        for f in [2u8, 4, 0x80] {
            let mut b = vec![1u8];
            b.extend(node(&s("Registry"), f, 0, 0, 0, 0));
            assert!(decode(&seal(&b)).is_err());
        }
        // Duplicate children, and a child that is also a tombstone.
        let mut b = vec![1u8];
        b.extend(node(&s("Registry"), 0, 0, 0, 0, 2));
        b.extend(node(&s("A"), 0, 0, 0, 0, 0));
        b.extend(node(&s("a"), 0, 0, 0, 0, 0));
        assert!(decode(&seal(&b)).is_err());
        let mut b = vec![1u8];
        b.extend(node(&s("Registry"), 0, 0, 0, 1, 1));
        b.extend(s("A"));
        b.extend(node(&s("a"), 0, 0, 0, 0, 0));
        assert!(decode(&seal(&b)).is_err());
        // Duplicate values (names fold equal), and a value that is also tombstoned.
        let value = |n: &str| {
            let mut v = s(n);
            v.extend_from_slice(&1u32.to_le_bytes());
            v.extend(s("x"));
            v
        };
        let mut b = vec![1u8];
        b.extend(node(&s("Registry"), 0, 2, 0, 0, 0));
        b.extend(value("V"));
        b.extend(value("v"));
        assert!(decode(&seal(&b)).is_err());
        let mut b = vec![1u8];
        b.extend(node(&s("Registry"), 0, 1, 1, 0, 0));
        b.extend(value("V"));
        b.extend(s("v"));
        assert!(decode(&seal(&b)).is_err());
        let mut b = vec![1u8];
        b.extend(node(&s("Registry"), 0, 1, 0, 0, 0));
        b.extend(value("V"));
        assert!(decode(&seal(&b)).is_ok());
        // Over-limit key name.
        let mut b = vec![1u8];
        b.extend(node(&s("Registry"), 0, 0, 0, 0, 1));
        b.extend(node(&s(&"k".repeat(256)), 0, 0, 0, 0, 0));
        assert!(decode(&seal(&b)).is_err());
        // Trailing bytes after the tree.
        let mut b = good.clone();
        b.push(0);
        assert!(decode(&seal(&b)).is_err());
        // Bad presence byte.
        assert!(decode(&seal(&[2])).is_err());
        let _ = Value {
            name: String::new(),
            ty: 0,
            data: vec![],
        };
    }

    #[test]
    fn write_depth_round_trips_and_decode_accepts_older_deeper_files() {
        // Debug-build recursion frames at 1024 levels outgrow the default 2 MiB test stack.
        std::thread::Builder::new()
            .stack_size(32 << 20)
            .spawn(write_depth_body)
            .unwrap()
            .join()
            .unwrap();
    }

    fn write_depth_body() {
        use crate::overlay::{RegError, MAX_WRITE_DEPTH};
        let chain = |n: usize| {
            let mut p = String::from(r"\Registry");
            for i in 0..n {
                p.push_str(&format!(r"\k{i}"));
            }
            p
        };
        let mut o = Overlay::new();
        let deepest = chain(MAX_WRITE_DEPTH);
        o.create_key(&deepest, false, false, 7).unwrap();
        o.set_value(&deepest, "v", 1, b"x", 8).unwrap();
        let back = decode(&encode(&o)).unwrap();
        assert_eq!(back.node(&deepest), o.node(&deepest));
        assert_eq!(back.node(&deepest).unwrap().values.len(), 1);

        // A file the old, uncapped writer could have made: `depth` nested "k" keys.
        let file = |depth: usize| {
            let mut b = vec![1u8];
            b.extend(node(&s("Registry"), 0, 0, 0, 0, 1));
            for _ in 1..depth {
                b.extend(node(&s("k"), 0, 0, 0, 0, 1));
            }
            b.extend(node(&s("k"), 0, 0, 0, 0, 0));
            seal(&b)
        };
        let old = decode(&file(MAX_WRITE_DEPTH + 100)).expect("deeper than the write cap decodes");
        // It re-encodes and reloads, but a new write that deep is refused.
        assert!(decode(&encode(&old)).is_ok());
        let mut p = String::from(r"\Registry");
        for _ in 0..MAX_WRITE_DEPTH + 100 {
            p.push_str(r"\k");
        }
        let mut old = old;
        assert!(old.node(&p).is_some());
        p.push_str(r"\new");
        assert_eq!(old.create_key(&p, false, false, 9), Err(RegError::TooDeep));

        assert!(decode(&file(MAX_DECODE_DEPTH)).is_ok());
        assert!(matches!(
            decode(&file(MAX_DECODE_DEPTH + 1)),
            Err(FormatError::Invalid("keys nested too deeply"))
        ));
    }

    #[test]
    fn multibyte_names_at_the_limits_round_trip() {
        for unit in ["\u{e9}", "\u{20ac}", "\u{1F600}"] {
            let w = unit.encode_utf16().count();
            let key = unit.repeat(MAX_KEY_NAME / w);
            let val = unit.repeat(MAX_VALUE_NAME / w);
            let path = format!(r"\Registry\{key}");
            let mut o = Overlay::new();
            o.create_key(&path, false, false, 1).unwrap();
            o.set_value(&path, &val, 1, b"d", 2).unwrap();
            let back = decode(&encode(&o)).unwrap();
            assert_eq!(back.node(&path), o.node(&path));
            assert_eq!(back.node(&path).unwrap().values[0].name, val);
        }
    }

    fn checksum(b: &[u8]) -> u64 {
        let mut sum = 0u64;
        for c in b.chunks(8) {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            sum = sum.wrapping_add(u64::from_le_bytes(w));
        }
        sum
    }
}
