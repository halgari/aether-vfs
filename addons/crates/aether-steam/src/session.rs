//! A CM session for the few requests that need one: depot keys, manifest
//! request codes, the CDN server list and CDN auth tokens. Everything goes
//! through the serializing actor in `cm`.
use crate::cdn::{CdnServer, CdnTokenSource};
use crate::cm::{Cm, Logon, Reply, Rpc, SessionConfig, SteamConnector};
use crate::credentials::SteamCredentials;
use crate::error::SteamError;
use crate::ids::{AppId, DepotId, DepotKey, ManifestId};
use crate::ticket::AppTicket;
use std::future::Future;
use std::pin::Pin;

/// A connected Steam session. Cheap to clone; the connection closes when the
/// last clone is dropped.
#[derive(Clone)]
pub struct SteamSession {
    cm: Cm,
    account: Option<String>,
}

fn unexpected(r: Reply) -> SteamError {
    // Only the variant name, never its payload: `Reply::CdnToken` carries a
    // CDN auth token, and other variants may carry other secrets later.
    let kind = match r {
        Reply::DepotKey(_) => "DepotKey",
        Reply::ManifestCode(_) => "ManifestCode",
        Reply::CdnServers(_) => "CdnServers",
        Reply::CdnToken(_) => "CdnToken",
        Reply::Manifests(_) => "Manifests",
        Reply::Ticket(_) => "Ticket",
        Reply::Owns(_) => "Owns",
    };
    SteamError::Protocol(format!("mismatched reply from the session task: {kind}"))
}

impl SteamSession {
    /// Log on anonymously. Enough for the CDN server list and free apps.
    pub async fn anonymous(cfg: SessionConfig) -> Result<Self, SteamError> {
        let connector = SteamConnector {
            logon: Logon::Anonymous,
            cell_id: cfg.cell_id,
        };
        Ok(SteamSession {
            cm: Cm::start(connector, cfg).await?,
            account: None,
        })
    }

    /// Log on with a saved refresh token. Fails with
    /// [`SteamError::LoginExpired`] when Steam no longer accepts it.
    pub async fn login(creds: &SteamCredentials, cfg: SessionConfig) -> Result<Self, SteamError> {
        let connector = SteamConnector {
            logon: Logon::Token(creds.clone()),
            cell_id: cfg.cell_id,
        };
        Ok(SteamSession {
            cm: Cm::start(connector, cfg).await?,
            account: Some(creds.account_name.clone()),
        })
    }

    #[cfg(test)]
    pub(crate) fn from_cm(cm: Cm, account: Option<String>) -> Self {
        SteamSession { cm, account }
    }

    /// The logged-on account, or `None` for an anonymous session.
    pub fn account(&self) -> Option<&str> {
        self.account.as_deref()
    }

    pub async fn depot_key(&self, app: AppId, depot: DepotId) -> Result<DepotKey, SteamError> {
        match self.cm.call(Rpc::DepotKey { app, depot }).await? {
            Reply::DepotKey(k) => Ok(k),
            r => Err(unexpected(r)),
        }
    }

    /// The code the CDN needs to serve `manifest`; 0 when Steam grants none.
    pub async fn manifest_request_code(
        &self,
        app: AppId,
        depot: DepotId,
        manifest: ManifestId,
    ) -> Result<u64, SteamError> {
        match self
            .cm
            .call(Rpc::ManifestCode {
                app,
                depot,
                manifest,
            })
            .await?
        {
            Reply::ManifestCode(c) => Ok(c),
            r => Err(unexpected(r)),
        }
    }

    /// CDN servers usable for `app`, filtered like DepotDownloader.
    pub async fn cdn_servers(&self, app: AppId) -> Result<Vec<CdnServer>, SteamError> {
        match self.cm.call(Rpc::CdnServers { app }).await? {
            Reply::CdnServers(s) => Ok(s),
            r => Err(unexpected(r)),
        }
    }

    pub async fn cdn_auth_token(
        &self,
        app: AppId,
        depot: DepotId,
        host: &str,
    ) -> Result<Option<String>, SteamError> {
        let rpc = Rpc::CdnToken {
            app,
            depot,
            host: host.to_string(),
        };
        match self.cm.call(rpc).await? {
            Reply::CdnToken(t) => Ok(t),
            r => Err(unexpected(r)),
        }
    }
}

impl SteamSession {
    /// An encrypted app ticket for `app`, minted by Steam for this
    /// account, with the 4 zero bytes of user data games send. Fails with
    /// [`SteamError::NotOwned`] when the account does not own `app`.
    pub async fn encrypted_app_ticket(&self, app: AppId) -> Result<AppTicket, SteamError> {
        let rpc = Rpc::EncryptedAppTicket {
            app,
            userdata: vec![0; 4],
        };
        match self.cm.call(rpc).await? {
            Reply::Ticket(t) => Ok(t),
            r => Err(unexpected(r)),
        }
    }

    /// Whether one of this account's licences grants `app` (a game, or a
    /// DLC such as the Anniversary Upgrade, 1746860). Read from the
    /// licence list Steam sent at logon and the packages' PICS info.
    pub async fn owns_app(&self, app: AppId) -> Result<bool, SteamError> {
        match self.cm.call(Rpc::OwnsApp { app }).await? {
            Reply::Owns(b) => Ok(b),
            r => Err(unexpected(r)),
        }
    }

    /// The current manifest of every depot of `app` on `branch` (usually
    /// `"public"`), from PICS. Works anonymously.
    pub async fn branch_manifests(
        &self,
        app: AppId,
        branch: &str,
    ) -> Result<Vec<(DepotId, ManifestId)>, SteamError> {
        let rpc = Rpc::BranchManifests {
            app,
            branch: branch.to_string(),
        };
        match self.cm.call(rpc).await? {
            Reply::Manifests(m) => Ok(m),
            r => Err(unexpected(r)),
        }
    }
}

impl CdnTokenSource for SteamSession {
    fn cdn_auth_token<'a>(
        &'a self,
        app: AppId,
        depot: DepotId,
        host: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, SteamError>> + Send + 'a>> {
        Box::pin(SteamSession::cdn_auth_token(self, app, depot, host))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::tests::{Behave, fake_with_cdn};

    #[test]
    fn unexpected_reply_never_leaks_a_cdn_token() {
        let err = unexpected(Reply::CdnToken(Some("SECRETVALUE".into())));
        assert!(!format!("{err}").contains("SECRETVALUE"));
        assert!(!format!("{err:?}").contains("SECRETVALUE"));
    }

    #[tokio::test]
    async fn each_request_gets_its_own_reply() {
        let server = CdnServer {
            host: "cdn.example".into(),
            port: 443,
            https: true,
        };
        let (cm, _) = fake_with_cdn(vec![Behave::Answer], vec![server.clone()]).await;
        let s = SteamSession::from_cm(cm, Some("alice".into()));
        assert_eq!(s.account(), Some("alice"));
        assert_eq!(
            s.depot_key(AppId(1), DepotId(7)).await.unwrap(),
            DepotKey([7; 32])
        );
        assert_eq!(
            s.manifest_request_code(AppId(1), DepotId(7), ManifestId(8))
                .await
                .unwrap(),
            77
        );
        assert_eq!(s.cdn_servers(AppId(1)).await.unwrap(), vec![server]);
        assert_eq!(
            s.cdn_auth_token(AppId(1), DepotId(7), "cdn.example")
                .await
                .unwrap(),
            Some("t".to_string())
        );
        assert!(
            s.branch_manifests(AppId(1), "public")
                .await
                .unwrap()
                .is_empty()
        );
        let t = s.encrypted_app_ticket(AppId(489830)).await.unwrap();
        assert_eq!(t.bytes().len(), 159);
        assert!(s.owns_app(AppId(1746860)).await.unwrap());
        assert!(!s.owns_app(AppId(1)).await.unwrap());
    }

    #[test]
    fn unexpected_reply_never_leaks_a_ticket() {
        let t = AppTicket::new(vec![0xCD; 8]);
        let err = unexpected(Reply::Ticket(t));
        for s in [format!("{err}"), format!("{err:?}")] {
            assert!(!s.contains("cdcd"), "{s}");
            assert!(s.contains("Ticket"), "{s}");
        }
    }
}
