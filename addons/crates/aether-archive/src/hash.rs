use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use std::fmt;

/// Wabbajack's file hash: xxHash64 with seed 0. In JSON it is base64 of the
/// little-endian bytes of the u64.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Xxh64(pub u64);

#[derive(Debug, thiserror::Error)]
#[error("invalid Wabbajack hash: {0:?}")]
pub struct HashParseError(pub String);

impl Xxh64 {
    pub fn of(bytes: &[u8]) -> Xxh64 {
        Xxh64(xxhash_rust::xxh64::xxh64(bytes, 0))
    }

    pub fn from_base64(s: &str) -> Result<Xxh64, HashParseError> {
        let bytes = STANDARD
            .decode(s)
            .map_err(|_| HashParseError(s.to_owned()))?;
        let arr: [u8; 8] = bytes.try_into().map_err(|_| HashParseError(s.to_owned()))?;
        Ok(Xxh64(u64::from_le_bytes(arr)))
    }

    pub fn to_base64(self) -> String {
        STANDARD.encode(self.0.to_le_bytes())
    }
}

impl fmt::Debug for Xxh64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Xxh64({})", self.to_base64())
    }
}

impl fmt::Display for Xxh64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base64())
    }
}

impl Serialize for Xxh64 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&self.to_base64())
        } else {
            s.serialize_u64(self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Xxh64 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Xxh64, D::Error> {
        if d.is_human_readable() {
            let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
            Xxh64::from_base64(&s).map_err(D::Error::custom)
        } else {
            Ok(Xxh64(u64::deserialize(d)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_is_little_endian_xxhash64() {
        // Real directive from TPF: an InlineFile whose 2634-byte blob hashes to this.
        let h = Xxh64::from_base64("8BW5nkg/aP4=").unwrap();
        assert_eq!(h.to_base64(), "8BW5nkg/aP4=");
        assert_eq!(Xxh64::of(b""), Xxh64(0xef46db3751d8e999));
        assert_eq!(Xxh64::of(b"").to_base64(), "menYUTfbRu8=");
        assert_eq!(Xxh64::of(b"abc"), Xxh64(0x44bc2cf5ad770999));
        assert_eq!(Xxh64::of(b"abc").to_base64(), "mQl3rfUsvEQ=");
    }

    #[test]
    fn rejects_bad_base64_and_wrong_length() {
        assert!(Xxh64::from_base64("not base64!").is_err());
        assert!(Xxh64::from_base64("AAAA").is_err()); // 3 bytes
    }

    #[test]
    fn serde_json_is_base64_and_postcard_is_u64() {
        let h = Xxh64(0x44bc2cf5ad770999);
        assert_eq!(serde_json::to_string(&h).unwrap(), "\"mQl3rfUsvEQ=\"");
        assert_eq!(
            serde_json::from_str::<Xxh64>("\"mQl3rfUsvEQ=\"").unwrap(),
            h
        );
        let bytes = postcard::to_allocvec(&h).unwrap();
        assert!(bytes.len() <= 10);
        assert_eq!(postcard::from_bytes::<Xxh64>(&bytes).unwrap(), h);
    }
}
