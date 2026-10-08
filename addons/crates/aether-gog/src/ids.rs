//! GOG identifiers. GOG's JSON writes ids sometimes as numbers and
//! sometimes as strings; both are accepted.
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};
use std::fmt;

/// A GOG product (a game or a DLC).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct ProductId(pub u64);

/// A build of a product (a game version on one OS).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct BuildId(pub String);

/// The OS a build is for; also the path segment GOG uses for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Windows,
    Osx,
    Linux,
}

impl Os {
    pub fn as_str(self) -> &'static str {
        match self {
            Os::Windows => "windows",
            Os::Osx => "osx",
            Os::Linux => "linux",
        }
    }
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ProductId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Display for BuildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A `u64` written as a JSON number or a decimal string.
pub(crate) fn u64_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    struct V;
    impl Visitor<'_> for V {
        type Value = u64;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an unsigned integer or a string holding one")
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
            Ok(v)
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<u64, E> {
            u64::try_from(v).map_err(|_| E::custom(format!("negative number {v}")))
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
            v.trim()
                .parse()
                .map_err(|_| E::custom(format!("not a number: {v:?}")))
        }
    }
    d.deserialize_any(V)
}

/// A string written as a JSON string or a number.
pub(crate) fn string_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    struct V;
    impl Visitor<'_> for V {
        type Value = String;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a string or a number")
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<String, E> {
            Ok(v.to_string())
        }
    }
    d.deserialize_any(V)
}

impl<'de> Deserialize<'de> for ProductId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        u64_lenient(d).map(ProductId)
    }
}

impl<'de> Deserialize<'de> for BuildId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        string_lenient(d).map(BuildId)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_accept_numbers_and_strings() {
        let p: Vec<ProductId> = serde_json::from_str(r#"[1207658691, "1207658692"]"#).unwrap();
        assert_eq!(p, [ProductId(1207658691), ProductId(1207658692)]);
        let b: Vec<BuildId> = serde_json::from_str(r#"[5645, "56452082907692588"]"#).unwrap();
        assert_eq!(b[0].0, "5645");
        assert_eq!(b[1].to_string(), "56452082907692588");
        assert!(serde_json::from_str::<ProductId>(r#""x""#).is_err());
        assert!(serde_json::from_str::<ProductId>("-1").is_err());
    }

    #[test]
    fn os_is_the_path_segment() {
        assert_eq!(Os::Osx.to_string(), "osx");
        let os: Os = serde_json::from_str(r#""windows""#).unwrap();
        assert_eq!(os, Os::Windows);
    }
}
