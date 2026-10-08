//! Haskill's own Steam sign-in, as small steps a CLI can drive: QR code, or
//! account name + password with Steam Guard (e-mail code, authenticator
//! code, or approval in the Steam mobile app). Every step is one request with
//! a deadline; steps take `&mut self`, so they can never overlap on the
//! connection.
use crate::cm::connect_ready;
use crate::credentials::SteamCredentials;
use crate::error::SteamError;
use base64::Engine;
use bytes::Bytes;
use prost::Message;
use std::future::Future;
use std::time::Duration;
use steamroom::client::{Ready, SteamClient};
use steamroom::enums::EResultError;
use steamroom::error::ConnectionError;
use steamroom::generated as pb;

/// `EAuthTokenPlatformType::SteamClient`: tokens the CM accepts at logon.
const PLATFORM_STEAM_CLIENT: i32 = 1;
/// `ESessionPersistence::Persistent`: a long-lived refresh token.
const PERSISTENT: i32 = 1;

/// Settings for a sign-in.
#[derive(Clone, Debug)]
pub struct LoginConfig {
    /// Shown to the user in Steam's list of authorized devices.
    pub device_name: String,
    /// Deadline for one attempt: connecting (if needed) plus the call
    /// itself. An idempotent request (e.g. polling) may be retried once
    /// after a lost connection or a timeout, so its worst case is twice
    /// this; a non-idempotent request (starting a session, submitting a
    /// Steam Guard code) is never retried and so never exceeds this.
    pub rpc_timeout: Duration,
}

impl Default for LoginConfig {
    fn default() -> Self {
        LoginConfig {
            device_name: "Haskill".into(),
            rpc_timeout: Duration::from_secs(30),
        }
    }
}

/// What a poll found.
#[derive(Debug)]
pub enum LoginPoll {
    /// Not approved yet; poll again after the poll interval.
    Pending,
    /// Steam issued a new QR challenge; render
    /// [`QrLogin::challenge_url`] again.
    NewChallenge,
    /// Signed in. Save these with [`CredentialFile::save`](crate::CredentialFile::save).
    Done(SteamCredentials),
}

/// Which Steam Guard confirmations this sign-in accepts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuardOffer {
    /// A code Steam e-mailed to the account.
    pub email_code: bool,
    /// A code from the Steam mobile authenticator.
    pub totp_code: bool,
    /// Approving the sign-in in the Steam mobile app.
    pub mobile_approval: bool,
    /// Clicking a link Steam e-mailed.
    pub email_approval: bool,
    /// The e-mail domain Steam names for `email_code`, e.g. `gmail.com`.
    pub email_domain: Option<String>,
}

impl GuardOffer {
    pub fn accepts_code(&self) -> bool {
        self.email_code || self.totp_code
    }
}

/// Which Steam Guard code [`GuardChallenge::submit_code`] is being given.
/// The caller picks, rather than Haskill guessing, because only the caller
/// knows which code the person actually has in hand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeKind {
    /// The code Steam e-mailed to the account.
    Email,
    /// The code from the Steam mobile authenticator (or another TOTP
    /// device Steam has linked).
    Device,
}

/// `allowed_confirmations` → `None` when no Steam Guard step is needed.
///
/// Steam Guard type 6 (`MachineToken`, a remembered-device confirmation)
/// and any other type this crate does not recognize carry nothing the user
/// can act on — no code to enter, no explicit approval link — but the
/// sign-in can still complete on its own once Steam finishes confirming
/// the device, so they are treated the same as [`GuardOffer::mobile_approval`]
/// et al: worth polling for rather than an immediate
/// [`SteamError::NoSupportedGuard`]. Only when *nothing at all* was offered
/// (an empty, non-"none needed" list never actually seen in practice, but
/// defensive either way) is that error still returned.
fn guard_offer(
    allowed: &[pb::CAuthenticationAllowedConfirmation],
) -> Result<Option<GuardOffer>, SteamError> {
    let mut o = GuardOffer::default();
    let mut none_needed = allowed.is_empty();
    let mut any_offered = false;
    for c in allowed {
        match c.confirmation_type.unwrap_or(0) {
            1 => none_needed = true,
            2 => {
                o.email_code = true;
                o.email_domain = c.associated_message.clone().filter(|m| !m.is_empty());
                any_offered = true;
            }
            3 => {
                o.totp_code = true;
                any_offered = true;
            }
            4 => {
                o.mobile_approval = true;
                any_offered = true;
            }
            5 => {
                o.email_approval = true;
                any_offered = true;
            }
            // Type 6 (MachineToken) or an unknown future type: no code or
            // approval link Haskill can show, but Steam may still confirm
            // the sign-in on its own — poll instead of refusing outright.
            _ => any_offered = true,
        }
    }
    if none_needed {
        return Ok(None);
    }
    if !any_offered {
        return Err(SteamError::NoSupportedGuard);
    }
    Ok(Some(o))
}

fn map_refusal(r: EResultError) -> SteamError {
    match r {
        EResultError::InvalidPassword => SteamError::InvalidPassword,
        EResultError::TwoFactorCodeMismatch | EResultError::Unknown(65) => {
            SteamError::InvalidGuardCode
        }
        EResultError::FileNotFound | EResultError::Expired => SteamError::AuthSessionExpired,
        EResultError::RateLimitExceeded => SteamError::Protocol(
            "Steam is limiting sign-in attempts; wait a few minutes and try again".into(),
        ),
        other => SteamError::Protocol(format!("Steam refused the sign-in: {other}")),
    }
}

/// Why one service-method attempt failed, before it is turned into a
/// [`SteamError`]. Kept separate from `SteamError` so callers that care
/// (like [`GuardChallenge::submit_code`], which treats a duplicate
/// submission as success) can match on the precise Steam refusal.
enum CallError {
    /// Steam answered with a non-OK `EResult`: the request was delivered
    /// and Steam decided. Never retried.
    Refused(EResultError),
    /// Anything else (no connection, a lost connection, a timeout, a
    /// malformed reply).
    Other(SteamError),
}

impl From<CallError> for SteamError {
    fn from(e: CallError) -> SteamError {
        match e {
            CallError::Refused(r) => map_refusal(r),
            CallError::Other(e) => e,
        }
    }
}

/// One connection able to run a pre-logon service call. Implemented for the
/// real `SteamClient<Ready>` and, in tests, for a fake — so the retry and
/// idempotency rules in [`AuthConn::call`] can be checked without a network.
trait AuthTransport: Send + Sync + 'static {
    fn call(
        &self,
        method: &str,
        body: Vec<u8>,
    ) -> impl Future<Output = Result<Bytes, TransportFail>> + Send;
}

/// Why a transport call failed.
enum TransportFail {
    /// Steam's `EResult` on the reply.
    Refused(EResultError),
    /// The connection was lost, or the reply could not be read. Whether the
    /// request reached Steam before that happened is unknown.
    Lost(String),
}

/// Opens connections an [`AuthConn`] can use.
trait AuthConnector: Send + Sync + 'static {
    type Conn: AuthTransport;
    fn connect(&self) -> impl Future<Output = Result<Self::Conn, SteamError>> + Send;
}

/// Connects to a live Steam CM and hands back a pre-logon session.
struct SteamAuthConnector;

impl AuthConnector for SteamAuthConnector {
    type Conn = SteamClient<Ready>;

    async fn connect(&self) -> Result<SteamClient<Ready>, SteamError> {
        connect_ready().await
    }
}

impl AuthTransport for SteamClient<Ready> {
    async fn call(&self, method: &str, body: Vec<u8>) -> Result<Bytes, TransportFail> {
        match self.call_service_method_non_authed(method, &body).await {
            Ok(resp) => Ok(resp.body),
            Err(steamroom::Error::Connection(ConnectionError::ServiceMethodFailed(r))) => {
                Err(TransportFail::Refused(r))
            }
            Err(e) => Err(TransportFail::Lost(e.to_string())),
        }
    }
}

/// A pre-logon CM connection for the Authentication service.
struct AuthConn<C: AuthConnector = SteamAuthConnector> {
    connector: C,
    client: Option<C::Conn>,
    timeout: Duration,
    /// Set for the duration of one write-then-read on `client`. If a call
    /// is cancelled (its future dropped) while this is set, the connection
    /// may have a half-written frame sitting on the wire; the next call
    /// drops it instead of reusing it, rather than risk desyncing the
    /// framing of whatever request reuses the socket next.
    in_flight: bool,
}

impl AuthConn<SteamAuthConnector> {
    fn new(cfg: &LoginConfig) -> Self {
        AuthConn {
            connector: SteamAuthConnector,
            client: None,
            timeout: cfg.rpc_timeout,
            in_flight: false,
        }
    }
}

impl<C: AuthConnector> AuthConn<C> {
    /// Connect if needed and run one send/receive. Cheap to call again
    /// immediately: it notices and discards a connection left mid-flight by
    /// a cancelled attempt before reusing (or reconnecting) it.
    async fn attempt(&mut self, method: &str, body: &[u8]) -> Result<Bytes, CallError> {
        if self.in_flight {
            self.client = None;
            self.in_flight = false;
        }
        if self.client.is_none() {
            self.client = Some(self.connector.connect().await.map_err(CallError::Other)?);
        }
        let c = self.client.as_ref().expect("just connected");
        self.in_flight = true;
        let result = c.call(method, body.to_vec()).await;
        self.in_flight = false;
        match result {
            Ok(bytes) => Ok(bytes),
            Err(TransportFail::Refused(r)) => Err(CallError::Refused(r)),
            Err(TransportFail::Lost(msg)) => {
                self.client = None;
                Err(CallError::Other(SteamError::Protocol(format!(
                    "Steam connection lost: {msg}"
                ))))
            }
        }
    }

    /// One service call with a deadline covering connecting (if needed) and
    /// the call. Idempotent calls (polling) are retried once after a lost
    /// connection or a timeout; auth sessions live server-side, so they
    /// survive the reconnect. Non-idempotent calls (starting a session,
    /// submitting a Steam Guard code) are attempted once only: after a
    /// timeout or a lost connection, Steam may already have received and
    /// acted on the request, so resending it could open a second session,
    /// send a second e-mail code, or reject a single-use code as already
    /// used. Only a refusal Steam actually returned (`Ok(Err(Refused))`) is
    /// definite enough to report without ambiguity, and that already
    /// returns immediately without retrying either way.
    async fn call<Req: Message, Resp: Message + Default>(
        &mut self,
        method: &str,
        req: &Req,
        idempotent: bool,
    ) -> Result<Resp, CallError> {
        let body = req.encode_to_vec();
        let attempts = if idempotent { 2 } else { 1 };
        let mut last = CallError::Other(SteamError::Protocol("no attempt made".into()));
        for _ in 0..attempts {
            match tokio::time::timeout(self.timeout, self.attempt(method, &body)).await {
                Ok(Ok(bytes)) => {
                    return Resp::decode(&*bytes).map_err(|e| {
                        CallError::Other(SteamError::Protocol(format!(
                            "bad reply to {method}: {e}"
                        )))
                    });
                }
                Ok(Err(CallError::Refused(r))) => return Err(CallError::Refused(r)),
                Ok(Err(other)) => last = other,
                Err(_) => {
                    last = CallError::Other(SteamError::Timeout {
                        what: "Steam's sign-in service",
                        after: self.timeout,
                    });
                }
            }
        }
        Err(last)
    }
}

/// Server-side sign-in state shared by the QR and password flows.
struct Pending<C: AuthConnector = SteamAuthConnector> {
    conn: AuthConn<C>,
    client_id: u64,
    request_id: Vec<u8>,
    interval: Duration,
    account: Option<String>,
    guard_data: Option<String>,
}

/// Steam's poll interval, sanity-checked: too small would hammer the
/// service, non-finite or non-positive makes no sense.
fn interval(secs: Option<f32>) -> Duration {
    match secs {
        Some(s) if s.is_finite() && s > 0.0 => Duration::from_secs_f32(s.clamp(1.0, 60.0)),
        _ => Duration::from_secs(5),
    }
}

impl<C: AuthConnector> Pending<C> {
    async fn poll(&mut self, challenge: Option<&mut String>) -> Result<LoginPoll, SteamError> {
        let req = pb::CAuthenticationPollAuthSessionStatusRequest {
            client_id: Some(self.client_id),
            request_id: Some(self.request_id.clone()),
            ..Default::default()
        };
        // Idempotent: polling again after a lost connection or a timeout
        // just asks the same question again.
        let r: pb::CAuthenticationPollAuthSessionStatusResponse = self
            .conn
            .call("Authentication.PollAuthSessionStatus#1", &req, true)
            .await?;
        interpret_poll(
            r,
            &mut self.client_id,
            challenge,
            self.account.as_deref(),
            self.guard_data.as_deref(),
        )
    }
}

/// Apply a poll response: follow a rotated client id or QR challenge, and
/// turn issued tokens into credentials.
fn interpret_poll(
    r: pb::CAuthenticationPollAuthSessionStatusResponse,
    client_id: &mut u64,
    challenge: Option<&mut String>,
    fallback_account: Option<&str>,
    prior_guard: Option<&str>,
) -> Result<LoginPoll, SteamError> {
    if let Some(id) = r.new_client_id.filter(|&id| id != 0) {
        *client_id = id;
    }
    if let Some(token) = r.refresh_token.filter(|t| !t.is_empty()) {
        let account = r
            .account_name
            .filter(|a| !a.is_empty())
            .or_else(|| fallback_account.map(str::to_owned))
            .ok_or_else(|| {
                SteamError::Protocol("Steam issued a token without an account name".into())
            })?;
        let guard = r
            .new_guard_data
            .filter(|g| !g.is_empty())
            .or(prior_guard.map(str::to_owned));
        return Ok(LoginPoll::Done(SteamCredentials::new(
            account, token, guard,
        )));
    }
    if let (Some(url), Some(c)) = (r.new_challenge_url.filter(|u| !u.is_empty()), challenge) {
        *c = url;
        return Ok(LoginPoll::NewChallenge);
    }
    Ok(LoginPoll::Pending)
}

/// Sign in by scanning a QR code with the Steam mobile app.
pub struct QrLogin {
    pending: Pending,
    challenge_url: String,
}

impl QrLogin {
    pub async fn begin(cfg: &LoginConfig) -> Result<Self, SteamError> {
        let mut conn = AuthConn::new(cfg);
        let req = pb::CAuthenticationBeginAuthSessionViaQrRequest {
            device_friendly_name: Some(cfg.device_name.clone()),
            platform_type: Some(PLATFORM_STEAM_CLIENT),
            ..Default::default()
        };
        // Non-idempotent: resending would open a second QR session.
        let r: pb::CAuthenticationBeginAuthSessionViaQrResponse = conn
            .call("Authentication.BeginAuthSessionViaQR#1", &req, false)
            .await?;
        let missing = |f: &str| SteamError::Protocol(format!("Steam's QR reply has no {f}"));
        Ok(QrLogin {
            challenge_url: r.challenge_url.ok_or_else(|| missing("challenge_url"))?,
            pending: Pending {
                conn,
                client_id: r.client_id.ok_or_else(|| missing("client_id"))?,
                request_id: r.request_id.ok_or_else(|| missing("request_id"))?,
                interval: interval(r.interval),
                account: None,
                guard_data: None,
            },
        })
    }

    /// The URL to show as a QR code (see [`render_qr`](crate::render_qr)).
    pub fn challenge_url(&self) -> &str {
        &self.challenge_url
    }

    pub fn poll_interval(&self) -> Duration {
        self.pending.interval
    }

    pub async fn poll(&mut self) -> Result<LoginPoll, SteamError> {
        self.pending.poll(Some(&mut self.challenge_url)).await
    }
}

/// Result of starting a password sign-in.
pub enum PasswordLogin {
    Done(SteamCredentials),
    NeedsGuard(GuardChallenge),
}

/// Start a password sign-in. Pass the `guard_data` saved from an earlier
/// sign-in of the same account so Steam can skip the e-mail code.
pub async fn begin_password_login(
    account: &str,
    password: &str,
    guard_data: Option<&str>,
    cfg: &LoginConfig,
) -> Result<PasswordLogin, SteamError> {
    let mut conn = AuthConn::new(cfg);
    // Idempotent: a public key lookup has no side effect.
    let key: pb::CAuthenticationGetPasswordRsaPublicKeyResponse = conn
        .call(
            "Authentication.GetPasswordRSAPublicKey#1",
            &pb::CAuthenticationGetPasswordRsaPublicKeyRequest {
                account_name: Some(account.to_string()),
            },
            true,
        )
        .await?;
    let (Some(m), Some(e)) = (key.publickey_mod.as_deref(), key.publickey_exp.as_deref()) else {
        return Err(SteamError::Protocol("Steam sent no password key".into()));
    };
    let encrypted = steamroom::crypto::rsa::encrypt_with_rsa_public_key(password.as_bytes(), m, e)
        .map_err(|e| SteamError::Protocol(format!("encrypting the password: {e}")))?;
    let req = pb::CAuthenticationBeginAuthSessionViaCredentialsRequest {
        account_name: Some(account.to_string()),
        encrypted_password: Some(base64::engine::general_purpose::STANDARD.encode(encrypted)),
        encryption_timestamp: key.timestamp,
        remember_login: Some(true),
        persistence: Some(PERSISTENT),
        platform_type: Some(PLATFORM_STEAM_CLIENT),
        device_friendly_name: Some(cfg.device_name.clone()),
        guard_data: guard_data.map(str::to_owned),
        ..Default::default()
    };
    // Non-idempotent: resending would open a second session and, if Steam
    // Guard needs an e-mail code, send a second one.
    let r: pb::CAuthenticationBeginAuthSessionViaCredentialsResponse = conn
        .call(
            "Authentication.BeginAuthSessionViaCredentials#1",
            &req,
            false,
        )
        .await?;
    let missing = |f: &str| SteamError::Protocol(format!("Steam's sign-in reply has no {f}"));
    let offer = guard_offer(&r.allowed_confirmations)?;
    let mut pending = Pending {
        conn,
        client_id: r.client_id.ok_or_else(|| missing("client_id"))?,
        request_id: r.request_id.ok_or_else(|| missing("request_id"))?,
        interval: interval(r.interval),
        account: Some(account.to_string()),
        guard_data: guard_data.map(str::to_owned),
    };
    let steam_id = r.steamid.ok_or_else(|| missing("steamid"))?;
    match offer {
        Some(offer) => Ok(PasswordLogin::NeedsGuard(GuardChallenge {
            pending,
            steam_id,
            offer,
        })),
        None => {
            // No Steam Guard: tokens arrive within a poll or two.
            for _ in 0..10 {
                if let LoginPoll::Done(c) = pending.poll(None).await? {
                    return Ok(PasswordLogin::Done(c));
                }
                tokio::time::sleep(pending.interval).await;
            }
            Err(SteamError::AuthSessionExpired)
        }
    }
}

/// A password sign-in waiting for Steam Guard.
pub struct GuardChallenge {
    pending: Pending,
    steam_id: u64,
    offer: GuardOffer,
}

/// A `DuplicateRequest` refusal on a Steam Guard code submission means
/// Steam already accepted an earlier submission of it: `AuthConn::call`
/// itself never resends a non-idempotent request, but a caller that retries
/// `submit_code` at a higher level can still hit this. Treat it as success,
/// not failure.
fn is_duplicate_submission(e: &CallError) -> bool {
    matches!(e, CallError::Refused(EResultError::DuplicateRequest))
}

impl GuardChallenge {
    pub fn offer(&self) -> &GuardOffer {
        &self.offer
    }

    pub fn poll_interval(&self) -> Duration {
        self.pending.interval
    }

    /// Submit an e-mail or authenticator code. `kind` must be one Steam
    /// actually offered (see [`GuardChallenge::offer`]).
    /// [`SteamError::InvalidGuardCode`] leaves the challenge usable: ask for
    /// the code again.
    pub async fn submit_code(&mut self, code: &str, kind: CodeKind) -> Result<(), SteamError> {
        let (code_type, offered) = match kind {
            CodeKind::Email => (2, self.offer.email_code),
            CodeKind::Device => (3, self.offer.totp_code),
        };
        if !offered {
            return Err(SteamError::NoSupportedGuard);
        }
        let req = pb::CAuthenticationUpdateAuthSessionWithSteamGuardCodeRequest {
            client_id: Some(self.pending.client_id),
            steamid: Some(self.steam_id),
            code: Some(code.trim().to_string()),
            code_type: Some(code_type),
        };
        // Non-idempotent: a code is single-use; resending after an
        // ambiguous failure could reject it as already used.
        match self
            .pending
            .conn
            .call::<_, pb::CAuthenticationUpdateAuthSessionWithSteamGuardCodeResponse>(
                "Authentication.UpdateAuthSessionWithSteamGuardCode#1",
                &req,
                false,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if is_duplicate_submission(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Ask whether the sign-in was approved (by a submitted code or out of
    /// band in the mobile app or by e-mail link).
    pub async fn poll(&mut self) -> Result<LoginPoll, SteamError> {
        self.pending.poll(None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn conf(t: i32, msg: Option<&str>) -> pb::CAuthenticationAllowedConfirmation {
        pb::CAuthenticationAllowedConfirmation {
            confirmation_type: Some(t),
            associated_message: msg.map(str::to_owned),
        }
    }

    #[test]
    fn guard_offer_reads_allowed_confirmations() {
        assert_eq!(guard_offer(&[]).unwrap(), None);
        assert_eq!(guard_offer(&[conf(1, None)]).unwrap(), None);
        let o = guard_offer(&[conf(2, Some("example.com")), conf(4, None)])
            .unwrap()
            .unwrap();
        assert!(o.email_code && o.mobile_approval && !o.totp_code && o.accepts_code());
        assert_eq!(o.email_domain.as_deref(), Some("example.com"));
        let o = guard_offer(&[conf(3, None), conf(4, None)])
            .unwrap()
            .unwrap();
        assert!(o.totp_code && o.mobile_approval);
        let o = guard_offer(&[conf(5, None)]).unwrap().unwrap();
        assert!(o.email_approval && !o.accepts_code());
        // Type 6 (MachineToken) and other unrecognized types offer nothing
        // to act on, but the sign-in can still complete on its own: poll
        // rather than fail outright.
        let o = guard_offer(&[conf(6, None)]).unwrap().unwrap();
        assert_eq!(o, GuardOffer::default());
        let o = guard_offer(&[conf(99, None)]).unwrap().unwrap();
        assert_eq!(o, GuardOffer::default());
    }

    fn resp() -> pb::CAuthenticationPollAuthSessionStatusResponse {
        pb::CAuthenticationPollAuthSessionStatusResponse::default()
    }

    #[test]
    fn poll_pending_new_challenge_and_rotated_client_id() {
        let mut id = 1;
        let mut url = String::from("https://s.team/q/1/old");
        assert!(matches!(
            interpret_poll(resp(), &mut id, Some(&mut url), None, None).unwrap(),
            LoginPoll::Pending
        ));
        let r = pb::CAuthenticationPollAuthSessionStatusResponse {
            new_client_id: Some(9),
            new_challenge_url: Some("https://s.team/q/1/new".into()),
            ..resp()
        };
        assert!(matches!(
            interpret_poll(r, &mut id, Some(&mut url), None, None).unwrap(),
            LoginPoll::NewChallenge
        ));
        assert_eq!((id, url.as_str()), (9, "https://s.team/q/1/new"));
    }

    #[test]
    fn poll_done_builds_credentials() {
        let mut id = 1;
        let r = pb::CAuthenticationPollAuthSessionStatusResponse {
            refresh_token: Some("eyA.refresh".into()),
            access_token: Some("eyA.access".into()),
            account_name: Some("alice".into()),
            new_guard_data: Some("guard".into()),
            ..resp()
        };
        let LoginPoll::Done(c) = interpret_poll(r, &mut id, None, None, None).unwrap() else {
            panic!()
        };
        assert_eq!(
            (c.account_name.as_str(), c.refresh_token()),
            ("alice", "eyA.refresh")
        );
        assert_eq!(c.guard_data.as_deref(), Some("guard"));
        // Password flow: account from the request, earlier guard data kept.
        let r = pb::CAuthenticationPollAuthSessionStatusResponse {
            refresh_token: Some("t".into()),
            ..resp()
        };
        let LoginPoll::Done(c) =
            interpret_poll(r, &mut id, None, Some("bob"), Some("old")).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (c.account_name.as_str(), c.guard_data.as_deref()),
            ("bob", Some("old"))
        );
    }

    #[test]
    fn refusals_map_to_user_facing_errors() {
        assert!(matches!(
            map_refusal(EResultError::InvalidPassword),
            SteamError::InvalidPassword
        ));
        assert!(matches!(
            map_refusal(EResultError::TwoFactorCodeMismatch),
            SteamError::InvalidGuardCode
        ));
        assert!(matches!(
            map_refusal(EResultError::Unknown(65)),
            SteamError::InvalidGuardCode
        ));
        assert!(matches!(
            map_refusal(EResultError::Expired),
            SteamError::AuthSessionExpired
        ));
        assert!(
            map_refusal(EResultError::RateLimitExceeded)
                .to_string()
                .contains("wait")
        );
    }

    #[test]
    fn poll_interval_is_sane() {
        assert_eq!(interval(Some(2.5)), Duration::from_millis(2500));
        assert_eq!(interval(None), Duration::from_secs(5));
        assert_eq!(interval(Some(f32::NAN)), Duration::from_secs(5));
        assert_eq!(interval(Some(-1.0)), Duration::from_secs(5));
        assert_eq!(interval(Some(1e9)), Duration::from_secs(60));
        assert_eq!(interval(Some(0.001)), Duration::from_secs(1));
    }

    #[test]
    fn duplicate_guard_code_submission_counts_as_success() {
        assert!(is_duplicate_submission(&CallError::Refused(
            EResultError::DuplicateRequest
        )));
        assert!(!is_duplicate_submission(&CallError::Refused(
            EResultError::InvalidPassword
        )));
        assert!(!is_duplicate_submission(&CallError::Other(
            SteamError::NoSupportedGuard
        )));
    }

    #[tokio::test]
    async fn submit_code_rejects_a_kind_steam_did_not_offer() {
        let mut gc = GuardChallenge {
            pending: Pending {
                conn: AuthConn::new(&LoginConfig::default()),
                client_id: 1,
                request_id: vec![],
                interval: Duration::from_secs(5),
                account: None,
                guard_data: None,
            },
            steam_id: 1,
            offer: GuardOffer {
                email_code: true,
                totp_code: false,
                mobile_approval: false,
                email_approval: false,
                email_domain: None,
            },
        };
        // Rejected before any network use: no connection is ever made.
        let err = gc
            .submit_code("123456", CodeKind::Device)
            .await
            .unwrap_err();
        assert!(matches!(err, SteamError::NoSupportedGuard));
    }

    // -- fakes for AuthConn's retry/idempotency/cancel-safety rules --

    #[derive(Clone, Copy)]
    enum Behave {
        Answer,
        Hang,
    }

    #[derive(Default)]
    struct Counts {
        connects: AtomicUsize,
        calls: AtomicUsize,
    }

    struct FakeConnector {
        /// Behavior of connection `n` (0-based); missing entries answer.
        script: Vec<Behave>,
        counts: Arc<Counts>,
    }

    struct FakeConn {
        behave: Behave,
        counts: Arc<Counts>,
    }

    impl AuthConnector for FakeConnector {
        type Conn = FakeConn;
        async fn connect(&self) -> Result<FakeConn, SteamError> {
            let n = self.counts.connects.fetch_add(1, Ordering::SeqCst);
            Ok(FakeConn {
                behave: *self.script.get(n).unwrap_or(&Behave::Answer),
                counts: self.counts.clone(),
            })
        }
    }

    impl AuthTransport for FakeConn {
        async fn call(&self, _method: &str, _body: Vec<u8>) -> Result<Bytes, TransportFail> {
            self.counts.calls.fetch_add(1, Ordering::SeqCst);
            match self.behave {
                Behave::Answer => Ok(Bytes::new()),
                Behave::Hang => std::future::pending().await,
            }
        }
    }

    fn fake(script: Vec<Behave>) -> (AuthConn<FakeConnector>, Arc<Counts>) {
        let counts = Arc::new(Counts::default());
        let conn = AuthConn {
            connector: FakeConnector {
                script,
                counts: counts.clone(),
            },
            client: None,
            timeout: Duration::from_millis(15),
            in_flight: false,
        };
        (conn, counts)
    }

    #[tokio::test]
    async fn non_idempotent_call_is_not_retried_after_a_timeout() {
        let (mut conn, counts) = fake(vec![Behave::Hang, Behave::Answer]);
        let req = pb::CAuthenticationBeginAuthSessionViaCredentialsRequest::default();
        let r: Result<pb::CAuthenticationBeginAuthSessionViaCredentialsResponse, CallError> = conn
            .call(
                "Authentication.BeginAuthSessionViaCredentials#1",
                &req,
                false,
            )
            .await;
        assert!(matches!(
            r,
            Err(CallError::Other(SteamError::Timeout { .. }))
        ));
        assert_eq!(
            counts.calls.load(Ordering::SeqCst),
            1,
            "a non-idempotent call must not be resent after an ambiguous failure"
        );
    }

    #[tokio::test]
    async fn idempotent_call_is_retried_once_after_a_timeout() {
        let (mut conn, counts) = fake(vec![Behave::Hang, Behave::Answer]);
        let req = pb::CAuthenticationPollAuthSessionStatusRequest::default();
        let r: Result<pb::CAuthenticationPollAuthSessionStatusResponse, CallError> = conn
            .call("Authentication.PollAuthSessionStatus#1", &req, true)
            .await;
        assert!(r.is_ok());
        assert_eq!(counts.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancelling_a_call_drops_the_connection_before_reuse() {
        let (mut conn, counts) = fake(vec![Behave::Hang, Behave::Answer]);
        let req = pb::CAuthenticationPollAuthSessionStatusRequest::default();

        // Cancel the call from outside, mid-flight on the (hanging) first
        // connection, well before AuthConn's own longer per-attempt
        // deadline would fire on its own.
        let _ = tokio::time::timeout(
            Duration::from_millis(2),
            conn.call::<_, pb::CAuthenticationPollAuthSessionStatusResponse>(
                "Authentication.PollAuthSessionStatus#1",
                &req,
                true,
            ),
        )
        .await;
        assert_eq!(counts.connects.load(Ordering::SeqCst), 1);

        // A fresh call must not reuse that connection (its framing state is
        // unknown): it should reconnect and succeed on the scripted second
        // (answering) connection.
        let r: Result<pb::CAuthenticationPollAuthSessionStatusResponse, CallError> = conn
            .call("Authentication.PollAuthSessionStatus#1", &req, true)
            .await;
        assert!(r.is_ok());
        assert_eq!(
            counts.connects.load(Ordering::SeqCst),
            2,
            "the cancelled, possibly half-written connection was dropped rather than reused"
        );
    }
}
