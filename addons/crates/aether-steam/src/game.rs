//! A game version as a list of depot manifests, to readable
//! [`SteamDepotFile`]s found by game-relative path (Windows separators,
//! any case, as Wabbajack's `GameFile` paths are).
use crate::content::SteamContent;
use crate::error::SteamError;
use crate::ids::{AppId, DepotId, ManifestId};
use crate::manifest::{DepotManifest, FileEntry};
use crate::reader::{DepotReader, SteamDepotFile};
use std::sync::Arc;

/// Every depot of one game version, ready to open files from.
pub struct SteamGame {
    app: AppId,
    depots: Vec<(Arc<DepotManifest>, DepotReader)>,
}

impl SteamGame {
    /// Load (from cache, or Steam) the manifests of `depots` of `app`.
    /// Earlier depots win when a path appears in more than one.
    pub async fn from_depots(
        content: &SteamContent,
        app: AppId,
        depots: &[(DepotId, ManifestId)],
    ) -> Result<Self, SteamError> {
        let mut out = Vec::with_capacity(depots.len());
        for &(depot, id) in depots {
            let manifest = content.manifest(app, depot, id).await?;
            let reader = content.reader(app, depot).await?;
            out.push((manifest, reader));
        }
        Ok(SteamGame { app, depots: out })
    }

    pub fn app(&self) -> AppId {
        self.app
    }

    /// Where a Wabbajack `GameFile` path (backslashes, any case) lives.
    pub fn locate(&self, game_file: &str) -> Option<(DepotId, &FileEntry)> {
        self.depots
            .iter()
            .find_map(|(m, _)| m.file(game_file).map(|f| (m.depot(), f)))
    }

    /// Open a Wabbajack `GameFile` path for reading.
    pub fn open_file(&self, game_file: &str) -> Result<SteamDepotFile, SteamError> {
        for (m, reader) in &self.depots {
            if m.file(game_file).is_some() {
                return reader.open(m.clone(), game_file);
            }
        }
        Err(SteamError::FileNotFound(game_file.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::SteamCache;
    use crate::cm::tests::fake_with_cdn;
    use crate::content::tests::{fast, serve_depot};
    use crate::ids::SKYRIM_SE;
    use crate::session::SteamSession;
    use crate::testutil::FixtureFile;
    use aether_archive::Xxh64;

    /// Depots 3 (manifest 30) and 1 (manifest 10) of Skyrim SE, each
    /// served by its own fake CDN.
    async fn setup() -> (
        tempfile::TempDir,
        SteamContent,
        Vec<crate::testutil::FakeCdn>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let exe: &[u8] = b"exe bytes in depot 1";
        let esm: &[u8] = b"TES4 master file in depot 3";
        let dup: &[u8] = b"depot 3 copy wins";
        let d3 = serve_depot(
            3,
            30,
            &[
                FixtureFile {
                    path: "Data\\Skyrim.esm",
                    data: esm,
                    chunk: 10,
                },
                FixtureFile {
                    path: "shared.txt",
                    data: dup,
                    chunk: 64,
                },
            ],
        )
        .await;
        let d1 = serve_depot(
            1,
            10,
            &[
                FixtureFile {
                    path: "SkyrimSE.exe",
                    data: exe,
                    chunk: 7,
                },
                FixtureFile {
                    path: "shared.txt",
                    data: b"depot 1 copy",
                    chunk: 64,
                },
            ],
        )
        .await;
        // One CDN list for the app; each fake only knows its own depot, so the
        // reader fails over to the other when it asks the wrong one.
        let (cm, _) = fake_with_cdn(vec![], vec![d3.server(), d1.server()]).await;
        let (s, c) = fast();
        let content = SteamContent::new(SteamCache::new(dir.path().join("steam")), None, s, c)
            .with_session(SteamSession::from_cm(cm, Some("alice".into())));
        (dir, content, vec![d3, d1])
    }

    #[tokio::test]
    async fn opens_game_files_by_wabbajack_path() {
        let (_dir, content, _cdns) = setup().await;
        let depots = [(DepotId(3), ManifestId(30)), (DepotId(1), ManifestId(10))];
        let game = SteamGame::from_depots(&content, SKYRIM_SE, &depots)
            .await
            .unwrap();
        assert_eq!(game.app(), AppId(489830));
        let esm = game.open_file("data\\SKYRIM.ESM").unwrap();
        assert_eq!(esm.depot(), DepotId(3));
        assert_eq!(
            esm.read_range(0, esm.len()).await.unwrap(),
            b"TES4 master file in depot 3"
        );
        let exe = game.open_file("SkyrimSE.exe").unwrap();
        exe.verify_xxh64(Xxh64::of(b"exe bytes in depot 1"))
            .await
            .unwrap();
        assert_eq!(
            game.locate("shared.txt").unwrap().0,
            DepotId(3),
            "first depot wins"
        );
        assert!(matches!(
            game.open_file("Data\\Missing.esp"),
            Err(SteamError::FileNotFound(_))
        ));
    }
}
