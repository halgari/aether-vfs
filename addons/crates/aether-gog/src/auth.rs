//! OAuth login: the browser URL, exchanging the pasted code for tokens,
//! and refreshing them.
//!
//! Ported from NexusMods.App `src/NexusMods.Networking.GOG/Client.cs`
//! (GPL-3.0), with the paste-the-redirect flow of heroic-gogdl/minigalaxy.
use aether_net::http::{check, read_body_max};
use aether_net::{Http, SourceError};
use serde::Deserialize;
use url::Url;
use zeroize::{Zeroize, Zeroizing};

use crate::config::{GogConfig, endpoint};
use crate::credentials::{GogCredentials, now_unix};
use crate::error::GogError;
use crate::ids::{string_lenient, u64_lenient};

const MAX_TOKEN_BODY: u64 = 64 << 10;

/// The address to open in a browser to log in to GOG. After login GOG
/// shows a page whose address carries the code for [`complete_login`].
pub fn login_url(cfg: &GogConfig) -> Url {
    let mut u = endpoint(&cfg.auth_base, "/auth");
    u.query_pairs_mut()
        .append_pair("client_id", &cfg.client_id)
        .append_pair("redirect_uri", &cfg.redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("layout", "client2");
    u
}

/// Exchange what the user pasted — the whole address of the page GOG
/// showed after login, or just its `code` — for tokens, save them to
/// `cfg.credentials` and return them.
pub async fn complete_login(
    http: &Http,
    cfg: &GogConfig,
    pasted: &str,
) -> Result<GogCredentials, GogError> {
    let code = Zeroizing::new(extract_code(pasted)?);
    let creds = match token_request(
        http,
        cfg,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &cfg.redirect_uri),
        ],
    )
    .await?
    {
        Token::Issued(c) => c,
        Token::Refused(status) => {
            return Err(GogError::Login(format!(
                "GOG refused the login code (HTTP {status}); a code works once and only for a \
                 few minutes, so log in again and paste the new address"
            )));
        }
    };
    creds.save(&cfg.credentials)?;
    Ok(creds)
}

/// New tokens for `creds`. A refused refresh token is
/// [`GogError::LoginExpired`].
pub(crate) async fn refresh(
    http: &Http,
    cfg: &GogConfig,
    creds: &GogCredentials,
) -> Result<GogCredentials, GogError> {
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", creds.refresh_token()),
    ];
    match token_request(http, cfg, &params).await? {
        Token::Issued(c) => Ok(c),
        Token::Refused(status) => Err(GogError::LoginExpired(format!(
            "the saved GOG login has expired or was revoked (GOG refused to refresh it, \
             HTTP {status}); log in to GOG again"
        ))),
    }
}

/// The code in `pasted`: a redirect URL's `code` parameter, or the bare code.
fn extract_code(pasted: &str) -> Result<String, GogError> {
    let s = pasted.trim();
    if s.is_empty() {
        return Err(GogError::Login("nothing was pasted".into()));
    }
    if let Ok(u) = Url::parse(s)
        && matches!(u.scheme(), "http" | "https")
    {
        return u
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.into_owned())
            .filter(|c| !c.is_empty())
            .ok_or_else(|| {
                GogError::Login(
                    "the pasted address has no `code`; finish logging in, then paste the \
                     address of the page GOG shows afterwards"
                        .into(),
                )
            });
    }
    if s.chars()
        .any(|c| c.is_whitespace() || matches!(c, '?' | '&' | '/' | '=' | '#'))
    {
        return Err(GogError::Login(
            "that is neither a login code nor the address of GOG's login-success page".into(),
        ));
    }
    Ok(s.to_string())
}

enum Token {
    Issued(GogCredentials),
    /// GOG answered 4xx: the grant (code or refresh token) is no good.
    Refused(u16),
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    #[serde(deserialize_with = "u64_lenient")]
    expires_in: u64,
    #[serde(deserialize_with = "string_lenient")]
    user_id: String,
}

impl Drop for TokenResponse {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

/// `GET {auth}/token?client_id=…&client_secret=…&{params}`, as GOG's token
/// endpoint wants (a GET, not a POST). Network errors and 5xx are retried
/// per the [`Http`]'s policy; errors never show the query (it holds the
/// secret, the code or the refresh token).
async fn token_request(
    http: &Http,
    cfg: &GogConfig,
    params: &[(&str, &str)],
) -> Result<Token, GogError> {
    let mut url = endpoint(&cfg.auth_base, "/token");
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", &cfg.client_id)
            .append_pair("client_secret", &cfg.client_secret);
        for (k, v) in params {
            q.append_pair(k, v);
        }
    }
    let job = http.start("gog token", None);
    let r = http
        .config()
        .retry
        .run(&job, || async {
            let _permit = http.permit(&url).await;
            let resp = http
                .api_client()
                .get(url.as_str())
                .send()
                .await
                .map_err(|e| SourceError::network(&url, e))?;
            read_body_max(check(resp, &url).await?, &url, &job, MAX_TOKEN_BODY).await
        })
        .await;
    let body = match job.complete(r) {
        Ok(b) => Zeroizing::new(b),
        Err(SourceError::Status { status, .. }) if (400..500).contains(&status) => {
            return Ok(Token::Refused(status));
        }
        Err(SourceError::NotFound { .. }) => return Ok(Token::Refused(404)),
        Err(e) => return Err(e.into()),
    };
    let mut t: TokenResponse =
        serde_json::from_slice(&body).map_err(|e| GogError::json("GOG token response", e))?;
    Ok(Token::Issued(GogCredentials::new(
        std::mem::take(&mut t.user_id),
        std::mem::take(&mut t.access_token),
        std::mem::take(&mut t.refresh_token),
        now_unix().saturating_add(t.expires_in),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_come_from_urls_or_stand_alone() {
        let u = "https://embed.gog.com/on_login_success?origin=client&code=AbC-12_x";
        assert_eq!(extract_code(u).unwrap(), "AbC-12_x");
        assert_eq!(extract_code(" AbC-12_x\n").unwrap(), "AbC-12_x");
        for bad in [
            "",
            "https://embed.gog.com/on_login_success?origin=client",
            "a b",
            "x?y",
        ] {
            assert!(extract_code(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn login_url_has_no_secret() {
        let cfg = GogConfig::new("/c", "/l");
        let u = login_url(&cfg);
        assert!(
            u.as_str()
                .starts_with("https://auth.gog.com/auth?client_id=46899977096215655")
        );
        assert!(!u.as_str().contains(&cfg.client_secret));
    }
}
