//! GOG Galaxy content-system v2 access: OAuth login (paste the redirect
//! URL), builds, build details and depot manifests (cached on disk), and
//! random-access reads of depot files from GOG's CDN, built on `aether-net`
//! for connection limits, retries and events.
//!
//! Ported from NexusMods.App (`src/NexusMods.Networking.GOG`, GPL-3.0),
//! <https://github.com/Nexus-Mods/NexusMods.App>.
mod api;
mod auth;
mod config;
mod content;
mod credentials;
mod error;
mod fsutil;
mod ids;
mod manifest;
mod reader;

pub use auth::{complete_login, login_url};
pub use config::{GALAXY_CLIENT_ID, GALAXY_CLIENT_SECRET, GALAXY_REDIRECT_URI, GogConfig};
pub use content::GogContent;
pub use credentials::GogCredentials;
pub use error::GogError;
pub use ids::{BuildId, Os, ProductId};
pub use manifest::{Build, BuildDetails, Chunk, DepotItem, DepotManifest, DepotRef, SfcRef};
pub use reader::{BlockingGogFile, GogDepotFile};
