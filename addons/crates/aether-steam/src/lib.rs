//! Steam depot access: its own Steam login, depot keys and
//! manifests cached on disk, and random-access reads of depot files from the
//! Steam CDN. Built on `steamroom` (pinned), whose types never appear in this
//! crate's public API.
mod auth;
mod cache;
mod cdn;
mod chunk;
mod cm;
mod content;
mod credentials;
mod error;
mod fsutil;
mod game;
mod ids;
mod licenses;
mod login;
mod manifest;
mod reader;
mod session;
#[cfg(test)]
mod testutil;
mod ticket;

pub use auth::{
    CodeKind, GuardChallenge, GuardOffer, LoginConfig, LoginPoll, PasswordLogin, QrLogin,
    begin_password_login,
};
pub use cache::SteamCache;
pub use cdn::{CdnConfig, CdnEnd, CdnObserver, CdnRequest, CdnServer, CdnTokenSource};
pub use cm::SessionConfig;
pub use content::{CDN_SERVER_MAX_AGE, SteamContent};
pub use credentials::{CredentialFile, SteamCredentials};
pub use error::SteamError;
pub use game::SteamGame;
pub use ids::*;
pub use login::{LoginMethod, LoginPrompter, login_interactive, render_qr};
pub use manifest::{ChunkRef, DepotManifest, FileEntry, fold_path};
pub use reader::{BlockingDepotFile, DepotReader, SteamDepotFile};
pub use session::SteamSession;
pub use ticket::AppTicket;
