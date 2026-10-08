//! Requests that carry the login's bearer token. An access token about to
//! expire is refreshed first; one GOG rejects (401) is refreshed exactly
//! once and the request tried again.
use aether_net::error::redact;
use aether_net::http::{check, read_body_max};
use aether_net::{Http, SourceError};
use tokio::sync::Mutex;
use url::Url;
use zeroize::Zeroizing;

use crate::auth::refresh;
use crate::config::GogConfig;
use crate::credentials::{GogCredentials, now_unix};
use crate::error::GogError;

pub(crate) struct Api {
    http: Http,
    cfg: GogConfig,
    creds: Mutex<GogCredentials>,
}

impl Api {
    pub(crate) fn new(http: Http, cfg: GogConfig, creds: GogCredentials) -> Api {
        Api {
            http,
            cfg,
            creds: Mutex::new(creds),
        }
    }

    /// `GET url` with the bearer token: the body, at most `max` bytes.
    pub(crate) async fn get(&self, url: &Url, max: u64) -> Result<Vec<u8>, GogError> {
        let token = self.token().await?;
        match self.attempt(url, &token, max).await {
            Err(SourceError::Status { status: 401, .. }) => {}
            r => return Ok(r?),
        }
        self.refresh_unless_done(&token).await?;
        let token = self.token().await?;
        match self.attempt(url, &token, max).await {
            Err(SourceError::Status { status: 401, .. }) => Err(GogError::LoginExpired(
                "GOG rejected the saved login even after refreshing it (HTTP 401); log in to \
                 GOG again"
                    .into(),
            )),
            r => Ok(r?),
        }
    }

    /// The current access token, refreshed first if it is about to expire.
    async fn token(&self) -> Result<Zeroizing<String>, GogError> {
        let mut creds = self.creds.lock().await;
        if creds.needs_refresh_at(now_unix()) {
            self.refresh_locked(&mut creds).await?;
        }
        Ok(Zeroizing::new(creds.access_token().to_string()))
    }

    /// Refresh after `stale` was rejected, unless another request already
    /// replaced it.
    async fn refresh_unless_done(&self, stale: &str) -> Result<(), GogError> {
        let mut creds = self.creds.lock().await;
        if creds.access_token() == stale {
            self.refresh_locked(&mut creds).await?;
        }
        Ok(())
    }

    async fn refresh_locked(&self, creds: &mut GogCredentials) -> Result<(), GogError> {
        let new = refresh(&self.http, &self.cfg, creds).await?;
        if let Err(e) = new.save(&self.cfg.credentials) {
            // The new tokens still work for this session.
            tracing::warn!(error = %e, "could not save the refreshed GOG login");
        }
        *creds = new;
        Ok(())
    }

    /// One request (with the retry policy for transient failures).
    async fn attempt(&self, url: &Url, token: &str, max: u64) -> Result<Vec<u8>, SourceError> {
        let job = self.http.start(format!("gog {}", redact(url)), None);
        let r = self
            .http
            .config()
            .retry
            .run(&job, || async {
                let _permit = self.http.permit(url).await;
                let resp = self
                    .http
                    .api_client()
                    .get(url.clone())
                    .bearer_auth(token)
                    .send()
                    .await
                    .map_err(|e| SourceError::network(url, e))?;
                read_body_max(check(resp, url).await?, url, &job, max).await
            })
            .await;
        job.complete(r)
    }
}
