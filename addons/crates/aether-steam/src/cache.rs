//! On-disk Steam state under a caller-given directory: depot keys (they never
//! change), parsed manifests (immutable per id) and the CDN server list (short
//! lived). With keys and manifests cached, reading game files needs no CM
//! session at all.
//!
//! ```text
//! <root>/depot-keys/<depot>.key              32 raw bytes, mode 0600
//! <root>/manifests/<depot>_<manifest>.bin    DepotManifest::encode
//! <root>/cdn-servers-<app>.json              {fetched_at, servers}
//! ```
use crate::cdn::CdnServer;
use crate::error::SteamError;
use crate::fsutil::{read_optional, write_atomic};
use crate::ids::{AppId, DepotId, DepotKey, ManifestId};
use crate::manifest::DepotManifest;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct SteamCache {
    root: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct ServerList {
    fetched_at: u64,
    servers: Vec<CdnServer>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl SteamCache {
    /// Use `root` (created on first write).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        SteamCache { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn key_path(&self, depot: DepotId) -> PathBuf {
        self.root.join("depot-keys").join(format!("{depot}.key"))
    }

    fn manifest_path(&self, depot: DepotId, id: ManifestId) -> PathBuf {
        self.root
            .join("manifests")
            .join(format!("{depot}_{id}.bin"))
    }

    fn servers_path(&self, app: AppId) -> PathBuf {
        self.root.join(format!("cdn-servers-{app}.json"))
    }

    /// The cached key for `depot`; a file of the wrong length is ignored.
    pub fn depot_key(&self, depot: DepotId) -> Result<Option<DepotKey>, SteamError> {
        let bytes = read_optional(&self.key_path(depot))?;
        Ok(bytes.and_then(|b| <[u8; 32]>::try_from(b).ok().map(DepotKey)))
    }

    pub fn put_depot_key(&self, depot: DepotId, key: &DepotKey) -> Result<(), SteamError> {
        write_atomic(&self.key_path(depot), &key.0, true)?;
        Ok(())
    }

    /// Drop the cached key for `depot`, if any — e.g. after it failed to
    /// decrypt a manifest's file names, so the next `depot_key()` call asks
    /// Steam for a fresh one instead of repeating the same bad key forever.
    pub fn forget_depot_key(&self, depot: DepotId) -> Result<(), SteamError> {
        match std::fs::remove_file(self.key_path(depot)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SteamError::Io(e)),
        }
    }

    /// The cached manifest; a damaged or outdated file counts as a miss.
    pub fn manifest(
        &self,
        depot: DepotId,
        id: ManifestId,
    ) -> Result<Option<DepotManifest>, SteamError> {
        let Some(bytes) = read_optional(&self.manifest_path(depot, id))? else {
            return Ok(None);
        };
        let m = DepotManifest::decode(&bytes).filter(|m| m.depot() == depot && m.id() == id);
        if m.is_none() {
            tracing::warn!(%depot, %id, "ignoring damaged cached manifest");
        }
        Ok(m)
    }

    pub fn put_manifest(&self, m: &DepotManifest) -> Result<(), SteamError> {
        write_atomic(&self.manifest_path(m.depot(), m.id()), &m.encode(), false)?;
        Ok(())
    }

    /// Cached CDN servers for `app` no older than `max_age`.
    pub fn cdn_servers(
        &self,
        app: AppId,
        max_age: Duration,
    ) -> Result<Option<Vec<CdnServer>>, SteamError> {
        let Some(bytes) = read_optional(&self.servers_path(app))? else {
            return Ok(None);
        };
        let Ok(list) = serde_json::from_slice::<ServerList>(&bytes) else {
            return Ok(None);
        };
        let age = now_unix().saturating_sub(list.fetched_at);
        Ok((age <= max_age.as_secs() && !list.servers.is_empty()).then_some(list.servers))
    }

    /// Cached CDN servers for `app` regardless of age, even long expired.
    /// Last-resort fallback for when a fresh fetch (from a fresh or stale
    /// cache miss) fails, so a stale list still beats no list at all.
    pub fn cdn_servers_any_age(&self, app: AppId) -> Result<Option<Vec<CdnServer>>, SteamError> {
        self.cdn_servers(app, Duration::MAX)
    }

    pub fn put_cdn_servers(&self, app: AppId, servers: &[CdnServer]) -> Result<(), SteamError> {
        let list = ServerList {
            fetched_at: now_unix(),
            servers: servers.to_vec(),
        };
        let json = serde_json::to_vec(&list).map_err(|e| SteamError::Protocol(e.to_string()))?;
        write_atomic(&self.servers_path(app), &json, false)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::tests::file;

    #[test]
    fn depot_keys_round_trip_privately() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SteamCache::new(dir.path().join("steam"));
        assert_eq!(cache.depot_key(DepotId(489831)).unwrap(), None);
        cache
            .put_depot_key(DepotId(489831), &DepotKey([5; 32]))
            .unwrap();
        assert_eq!(
            cache.depot_key(DepotId(489831)).unwrap(),
            Some(DepotKey([5; 32]))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.path().join("steam/depot-keys/489831.key");
            assert_eq!(
                std::fs::metadata(p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::write(dir.path().join("steam/depot-keys/1.key"), b"short").unwrap();
        assert_eq!(cache.depot_key(DepotId(1)).unwrap(), None);
    }

    #[test]
    fn manifests_round_trip_and_damage_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SteamCache::new(dir.path());
        let m = DepotManifest::new(DepotId(3), ManifestId(4), vec![file("a\\b", &[2, 2])]).unwrap();
        assert!(cache.manifest(DepotId(3), ManifestId(4)).unwrap().is_none());
        cache.put_manifest(&m).unwrap();
        assert_eq!(cache.manifest(DepotId(3), ManifestId(4)).unwrap(), Some(m));
        let p = dir.path().join("manifests/3_4.bin");
        let mut bytes = std::fs::read(&p).unwrap();
        let n = bytes.len();
        bytes[n - 3] ^= 1;
        std::fs::write(&p, bytes).unwrap();
        assert!(cache.manifest(DepotId(3), ManifestId(4)).unwrap().is_none());
    }

    #[test]
    fn cdn_servers_expire() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SteamCache::new(dir.path());
        let s = vec![CdnServer {
            host: "h".into(),
            port: 443,
            https: true,
        }];
        cache.put_cdn_servers(AppId(1), &s).unwrap();
        assert_eq!(
            cache
                .cdn_servers(AppId(1), Duration::from_secs(3600))
                .unwrap(),
            Some(s)
        );
        assert_eq!(
            cache
                .cdn_servers(AppId(2), Duration::from_secs(3600))
                .unwrap(),
            None
        );
        let p = dir.path().join("cdn-servers-1.json");
        std::fs::write(
            &p,
            br#"{"fetched_at":0,"servers":[{"host":"h","port":443,"https":true}]}"#,
        )
        .unwrap();
        assert_eq!(
            cache
                .cdn_servers(AppId(1), Duration::from_secs(3600))
                .unwrap(),
            None
        );
    }
}
