//! The Steam CM connection. steamroom has no response demultiplexer: two
//! overlapping calls on one client can take each other's replies. So every
//! CM request goes through one actor task that runs them one at a time, each
//! with a deadline. A timeout or transport error drops the connection (a late
//! reply must never be read as the answer to the next request), reconnects,
//! and retries the request once.
use crate::cdn::CdnServer;
use crate::credentials::SteamCredentials;
use crate::error::SteamError;
use crate::ids::{AppId, DepotId, DepotKey, ManifestId};
use crate::ticket::{self, AppTicket};
use prost::Message;
use std::future::Future;
use std::time::Duration;
use steamroom::client::{LoggedIn, Ready, SteamClient};
use steamroom::connection::{CmServer, Protocol};
use steamroom::error::ConnectionError;
use tokio::sync::{mpsc, oneshot};

/// Deadlines for CM traffic.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// Deadline for discovering, connecting to and logging on to a CM.
    pub connect_timeout: Duration,
    /// Deadline for one request/response.
    pub rpc_timeout: Duration,
    /// Heartbeat period while idle.
    pub heartbeat: Duration,
    /// Steam cell id sent at logon and for CDN server discovery.
    pub cell_id: u32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            // connect_ready() fetches the Steam directory, then tries up to
            // three CM servers at 10s each — up to 30s for the CM tries
            // alone before the directory fetch is even counted. 30s was
            // tight enough to misfire as a spurious Timeout on an ordinary
            // slow connection; 45s comfortably fits directory fetch + 3x10s.
            connect_timeout: Duration::from_secs(45),
            rpc_timeout: Duration::from_secs(30),
            heartbeat: Duration::from_secs(9),
            cell_id: 0,
        }
    }
}

/// A request to the CM.
#[derive(Clone, Debug)]
pub(crate) enum Rpc {
    DepotKey {
        app: AppId,
        depot: DepotId,
    },
    ManifestCode {
        app: AppId,
        depot: DepotId,
        manifest: ManifestId,
    },
    CdnServers {
        app: AppId,
    },
    CdnToken {
        app: AppId,
        depot: DepotId,
        host: String,
    },
    BranchManifests {
        app: AppId,
        branch: String,
    },
    EncryptedAppTicket {
        app: AppId,
        userdata: Vec<u8>,
    },
    OwnsApp {
        app: AppId,
    },
}

impl Rpc {
    /// The deadline for the whole request. Ownership walks every licence
    /// in several round trips (each with its own deadline), so it gets
    /// more.
    fn deadline(&self, cfg: &SessionConfig) -> Duration {
        match self {
            Rpc::OwnsApp { .. } => cfg.rpc_timeout * 8,
            _ => cfg.rpc_timeout,
        }
    }

    fn what(&self) -> &'static str {
        match self {
            Rpc::DepotKey { .. } => "a depot key from Steam",
            Rpc::ManifestCode { .. } => "a manifest request code from Steam",
            Rpc::CdnServers { .. } => "the Steam CDN server list",
            Rpc::CdnToken { .. } => "a Steam CDN auth token",
            Rpc::BranchManifests { .. } => "app info from Steam",
            Rpc::EncryptedAppTicket { .. } => "an app ticket from Steam",
            Rpc::OwnsApp { .. } => "the account's licences from Steam",
        }
    }
}

#[derive(Debug)]
pub(crate) enum Reply {
    DepotKey(DepotKey),
    ManifestCode(u64),
    CdnServers(Vec<CdnServer>),
    CdnToken(Option<String>),
    Manifests(Vec<(DepotId, ManifestId)>),
    Ticket(AppTicket),
    Owns(bool),
}

/// Why a call failed: the connection is suspect (drop it, reconnect, retry
/// once), or Steam answered no (report it).
#[derive(Debug)]
pub(crate) enum CmFail {
    Connection(String),
    Refused(SteamError),
}

/// One logged-on CM connection.
pub(crate) trait CmConn: Send + Sync + 'static {
    fn call(&self, rpc: &Rpc) -> impl Future<Output = Result<Reply, CmFail>> + Send;
    fn heartbeat(&self) -> impl Future<Output = Result<(), CmFail>> + Send;
}

/// Opens logged-on connections.
pub(crate) trait Connector: Send + Sync + 'static {
    type Conn: CmConn;
    fn connect(&self) -> impl Future<Output = Result<Self::Conn, SteamError>> + Send;
}

struct Job {
    rpc: Rpc,
    reply: oneshot::Sender<Result<Reply, SteamError>>,
}

/// Handle to the actor. Dropping every handle ends the task and closes the
/// connection.
#[derive(Clone)]
pub(crate) struct Cm {
    tx: mpsc::Sender<Job>,
}

impl Cm {
    /// Connect now (so login problems surface here) and start the actor.
    pub(crate) async fn start<C: Connector>(
        connector: C,
        cfg: SessionConfig,
    ) -> Result<Cm, SteamError> {
        let conn = connect(&connector, &cfg).await?;
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(run(connector, rx, cfg, Some(conn)));
        Ok(Cm { tx })
    }

    pub(crate) async fn call(&self, rpc: Rpc) -> Result<Reply, SteamError> {
        let (reply, rx) = oneshot::channel();
        let closed = || SteamError::Protocol("Steam session task stopped".into());
        self.tx
            .send(Job { rpc, reply })
            .await
            .map_err(|_| closed())?;
        rx.await.map_err(|_| closed())?
    }
}

async fn connect<C: Connector>(c: &C, cfg: &SessionConfig) -> Result<C::Conn, SteamError> {
    tokio::time::timeout(cfg.connect_timeout, c.connect())
        .await
        .map_err(|_| SteamError::Timeout {
            what: "a Steam connection",
            after: cfg.connect_timeout,
        })?
}

/// `tokio::time::interval()` panics outright on a zero period, which would
/// otherwise silently kill the whole actor task (and with it every future
/// `Cm::call()`) the first time a misconfigured `SessionConfig` with
/// `heartbeat: Duration::ZERO` reached it.
fn clamped_heartbeat(configured: Duration) -> Duration {
    if configured.is_zero() {
        tracing::warn!("SessionConfig::heartbeat was zero; using 1s instead");
        Duration::from_secs(1)
    } else {
        configured
    }
}

async fn run<C: Connector>(
    connector: C,
    mut rx: mpsc::Receiver<Job>,
    cfg: SessionConfig,
    mut conn: Option<C::Conn>,
) {
    let mut beat = tokio::time::interval(clamped_heartbeat(cfg.heartbeat));
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    beat.tick().await; // the first tick is immediate
    loop {
        tokio::select! {
            job = rx.recv() => {
                let Some(job) = job else { return };
                let result = serve(&connector, &mut conn, &job.rpc, &cfg).await;
                let _ = job.reply.send(result);
            }
            _ = beat.tick(), if conn.is_some() => {
                let c = conn.as_ref().expect("guarded by is_some");
                let ok = matches!(tokio::time::timeout(cfg.rpc_timeout, c.heartbeat()).await, Ok(Ok(())));
                if !ok {
                    tracing::debug!("Steam heartbeat failed; will reconnect on next request");
                    conn = None;
                }
            }
        }
    }
}

async fn serve<C: Connector>(
    connector: &C,
    conn: &mut Option<C::Conn>,
    rpc: &Rpc,
    cfg: &SessionConfig,
) -> Result<Reply, SteamError> {
    let mut last = SteamError::Protocol("no attempt made".into());
    for _ in 0..2 {
        if conn.is_none() {
            *conn = Some(connect(connector, cfg).await?);
        }
        let c = conn.as_ref().expect("just connected");
        match tokio::time::timeout(rpc.deadline(cfg), c.call(rpc)).await {
            Ok(Ok(reply)) => return Ok(reply),
            Ok(Err(CmFail::Refused(e))) => return Err(e),
            Ok(Err(CmFail::Connection(msg))) => {
                *conn = None;
                last = SteamError::Protocol(format!("Steam connection lost: {msg}"));
            }
            Err(_) => {
                *conn = None;
                last = SteamError::Timeout {
                    what: rpc.what(),
                    after: rpc.deadline(cfg),
                };
            }
        }
    }
    Err(last)
}

/// The CDN server directory request for the session's own `cell_id` — the
/// same cell it logged on with — so the servers Steam hands back are picked
/// for that cell rather than the default one.
fn cdn_servers_request(
    cell_id: u32,
) -> steamroom::generated::CContentServerDirectoryGetServersForSteamPipeRequest {
    steamroom::generated::CContentServerDirectoryGetServersForSteamPipeRequest {
        cell_id: Some(cell_id),
        max_servers: Some(30),
        ..Default::default()
    }
}

/// Keep the servers DepotDownloader would use for `app`: type `CDN` or
/// `SteamCache`, not China-only, not a proxy front, and either open to every
/// app or listing `app`. Least-loaded first, duplicates dropped. Uses `vhost`
/// as the host name, like SteamKit.
pub(crate) fn select_servers(
    infos: &[steamroom::generated::CContentServerDirectoryServerInfo],
    app: AppId,
) -> Vec<CdnServer> {
    let mut picked: Vec<(f32, CdnServer)> = infos
        .iter()
        .filter(|s| matches!(s.r#type.as_deref(), Some("CDN") | Some("SteamCache")))
        .filter(|s| !s.steam_china_only.unwrap_or(false) && !s.use_as_proxy.unwrap_or(false))
        .filter(|s| s.allowed_app_ids.is_empty() || s.allowed_app_ids.contains(&app.0))
        .filter_map(|s| {
            let name = s
                .vhost
                .as_deref()
                .filter(|v| !v.is_empty())
                .or(s.host.as_deref())?;
            let https = matches!(
                s.https_support.as_deref(),
                Some("mandatory") | Some("optional")
            );
            let host = name.rsplit_once(':').map_or(name, |(h, _)| h).to_string();
            let port = if https { 443 } else { 80 };
            Some((
                s.weighted_load.unwrap_or(0.0),
                CdnServer { host, port, https },
            ))
        })
        .collect();
    picked.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<CdnServer> = Vec::new();
    for (_, s) in picked {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// How the real connector logs on.
#[derive(Clone)]
pub(crate) enum Logon {
    Anonymous,
    Token(SteamCredentials),
}

/// Connects to a live Steam CM over WebSocket.
pub(crate) struct SteamConnector {
    pub(crate) logon: Logon,
    pub(crate) cell_id: u32,
}

/// A logged-on CM connection together with the cell id it logged on with,
/// so later requests that want it (the CDN server directory lookup) send
/// the same cell id the session itself was opened with, rather than always
/// claiming the default cell.
pub(crate) struct LoggedInConn {
    client: SteamClient<LoggedIn>,
    cell_id: u32,
    /// From the logon response header: what an authenticated message
    /// steamroom has no helper for must carry (steamroom keeps its own
    /// copies private).
    steam_id: u64,
    session_id: i32,
    /// The package ids of the licence list the CM pushes after logon,
    /// captured by whichever of this crate's own reads sees it first.
    licenses: std::sync::Mutex<Option<Vec<u32>>>,
}

/// How long an ownership question waits for a licence list not yet seen
/// before giving up on this connection (the retry reconnects, and a fresh
/// connection's list is still queued for the first request to read).
const LICENSE_LIST_WAIT: Duration = Duration::from_secs(5);
/// Deadline for one round trip of an ownership question.
const ROUND_TRIP: Duration = Duration::from_secs(15);

/// Discover CMs and open a `Ready` (pre-logon) WebSocket connection, trying
/// up to three servers.
pub(crate) async fn connect_ready() -> Result<SteamClient<Ready>, SteamError> {
    let servers = CmServer::fetch()
        .await
        .map_err(|e| SteamError::Protocol(format!("cannot reach the Steam directory: {e}")))?;
    let mut last = String::from("the Steam directory listed no WebSocket servers");
    for server in servers
        .iter()
        .filter(|s| s.protocol == Protocol::WebSocket)
        .take(3)
    {
        let attempt = async {
            let t = steamroom::transport::websocket::WebSocketTransport::connect(server).await?;
            let (client, _unused) = SteamClient::connect_ws(t).await?;
            client.prepare().await
        };
        match tokio::time::timeout(Duration::from_secs(10), attempt).await {
            Ok(Ok(c)) => return Ok(c),
            Ok(Err(e)) => last = e.to_string(),
            Err(_) => last = "no answer within 10s".into(),
        }
    }
    Err(SteamError::Protocol(format!(
        "cannot connect to Steam: {last}"
    )))
}

impl Connector for SteamConnector {
    type Conn = LoggedInConn;

    async fn connect(&self) -> Result<LoggedInConn, SteamError> {
        use steamroom::client::msg::ClientMsg;
        use steamroom::generated::CMsgClientLogon;
        use steamroom::messages::EMsg;
        use steamroom::types::steam_id::SteamId;
        let client = connect_ready().await?;
        let mut logon = CMsgClientLogon {
            protocol_version: Some(steamroom::client::PROTOCOL_VERSION),
            cell_id: Some(self.cell_id),
            client_os_type: Some(20), // EOSType Windows 11, as steamroom sends
            ..Default::default()
        };
        let steam_id = match &self.logon {
            Logon::Anonymous => SteamId::from_parts(1, 10, 0, 0), // public, AnonUser
            Logon::Token(c) => {
                logon.account_name = Some(c.account_name.clone());
                logon.access_token = Some(c.refresh_token().to_string());
                SteamId::from_parts(1, 1, 1, 0) // public, Individual, desktop
            }
        };
        let body = logon.encode_to_vec();
        let mut msg = ClientMsg::with_body(EMsg::CLIENT_LOGON, &body);
        msg.header.steamid = Some(steam_id.raw());
        msg.header.client_sessionid = Some(0);
        match client.login(msg).await {
            Ok((client, resp)) => Ok(LoggedInConn {
                client,
                cell_id: self.cell_id,
                steam_id: resp.header.steamid.unwrap_or(0),
                session_id: resp.header.client_sessionid.unwrap_or(0),
                licenses: std::sync::Mutex::new(None),
            }),
            Err(steamroom::Error::Connection(ConnectionError::LogonFailed(r))) => {
                use steamroom::enums::EResultError as E;
                match (&self.logon, r) {
                    (
                        Logon::Token(c),
                        E::InvalidPassword | E::AccessDenied | E::Expired | E::Revoked,
                    ) => Err(SteamError::LoginExpired {
                        account: c.account_name.clone(),
                    }),
                    (_, r) => Err(SteamError::Protocol(format!(
                        "Steam refused the logon: {r}"
                    ))),
                }
            }
            Err(e) => Err(SteamError::Protocol(format!("Steam logon failed: {e}"))),
        }
    }
}

fn refused_or_lost(e: steamroom::Error) -> CmFail {
    match e {
        steamroom::Error::Connection(ConnectionError::ServiceMethodFailed(r)) => CmFail::Refused(
            SteamError::Protocol(format!("Steam refused the request: {r}")),
        ),
        steamroom::Error::ProtobufDecode(e) => {
            CmFail::Refused(SteamError::Protocol(format!("bad reply from Steam: {e}")))
        }
        other => CmFail::Connection(other.to_string()),
    }
}

impl CmConn for LoggedInConn {
    async fn call(&self, rpc: &Rpc) -> Result<Reply, CmFail> {
        use steamroom::depot as sr;
        match rpc {
            Rpc::DepotKey { app, depot } => {
                match self
                    .client
                    .get_depot_decryption_key(sr::DepotId(depot.0), sr::AppId(app.0))
                    .await
                {
                    Ok(k) => Ok(Reply::DepotKey(DepotKey(k.0))),
                    Err(steamroom::Error::Connection(ConnectionError::DepotAccessDenied(_))) => {
                        Err(CmFail::Refused(SteamError::AccessDenied(*depot)))
                    }
                    Err(e) => Err(refused_or_lost(e)),
                }
            }
            Rpc::ManifestCode {
                app,
                depot,
                manifest,
            } => {
                let r = self
                    .client
                    .get_manifest_request_code(
                        sr::AppId(app.0),
                        sr::DepotId(depot.0),
                        sr::ManifestId(manifest.0),
                        None,
                        None,
                    )
                    .await;
                match r {
                    Ok(code) => Ok(Reply::ManifestCode(code.unwrap_or(0))),
                    Err(steamroom::Error::Connection(ConnectionError::ServiceMethodFailed(_))) => {
                        Err(CmFail::Refused(SteamError::ManifestUnavailable {
                            depot: *depot,
                            manifest: *manifest,
                        }))
                    }
                    Err(e) => Err(refused_or_lost(e)),
                }
            }
            Rpc::CdnServers { app } => {
                use steamroom::generated::CContentServerDirectoryGetServersForSteamPipeResponse as Resp;
                let req = cdn_servers_request(self.cell_id);
                let resp = self
                    .client
                    .call_service_method(
                        "ContentServerDirectory.GetServersForSteamPipe#1",
                        &req.encode_to_vec(),
                    )
                    .await
                    .map_err(refused_or_lost)?;
                let r: Resp = resp.decode().map_err(|e| refused_or_lost(e.into()))?;
                Ok(Reply::CdnServers(select_servers(&r.servers, *app)))
            }
            Rpc::CdnToken { app, depot, host } => {
                let t = self
                    .client
                    .get_cdn_auth_token(sr::AppId(app.0), sr::DepotId(depot.0), host)
                    .await
                    .map_err(refused_or_lost)?;
                Ok(Reply::CdnToken(t.token.filter(|t| !t.is_empty())))
            }
            Rpc::BranchManifests { app, branch } => {
                let d = self
                    .client
                    .app_details(sr::AppId(app.0))
                    .await
                    .map_err(refused_or_lost)?;
                let list = d
                    .depots
                    .iter()
                    .filter_map(|dep| {
                        let m = dep.manifests.iter().find(|m| &m.branch == branch)?;
                        Some((DepotId(dep.id.0), ManifestId(m.manifest_id.0)))
                    })
                    .collect();
                Ok(Reply::Manifests(list))
            }
            Rpc::EncryptedAppTicket { app, userdata } => self.ticket(*app, userdata).await,
            Rpc::OwnsApp { app } => self.owns(*app).await.map(Reply::Owns),
        }
    }

    async fn heartbeat(&self) -> Result<(), CmFail> {
        self.client.send_heartbeat().await.map_err(refused_or_lost)
    }
}

impl LoggedInConn {
    /// A message with this session's header, for requests steamroom has no
    /// helper for.
    fn msg<'a>(&self, emsg: u32, body: &'a [u8]) -> steamroom::client::msg::ClientMsg<'a> {
        let mut m =
            steamroom::client::msg::ClientMsg::with_body(steamroom::messages::EMsg(emsg), body);
        m.header.steamid = Some(self.steam_id);
        m.header.client_sessionid = Some(self.session_id);
        m
    }

    async fn ticket(&self, app: AppId, userdata: &[u8]) -> Result<Reply, CmFail> {
        let body = ticket::request_body(app.0, userdata);
        let mut m = self.msg(ticket::EMSG_REQUEST, &body);
        m.header.jobid_source = Some(1);
        self.client.send_msg(&m).await.map_err(refused_or_lost)?;
        let body = self.recv(ticket::EMSG_RESPONSE).await?;
        ticket::parse_response(app.0, &body)
            .map(Reply::Ticket)
            .map_err(CmFail::Refused)
    }
}

/// Packages asked about per PICS request.
const PACKAGES_PER_REQUEST: usize = 100;

impl LoggedInConn {
    /// Whether a licence of this account grants `app`: the packages of the
    /// licence list, with their PICS access tokens, then their package
    /// info (every split response read, so none is left for a later
    /// request to mistake for its own).
    async fn owns(&self, app: AppId) -> Result<bool, CmFail> {
        use steamroom::generated::CMsgClientPicsProductInfoRequest as Req;
        use steamroom::generated::CMsgClientPicsProductInfoResponse as Resp;
        use steamroom::generated::c_msg_client_pics_product_info_request::PackageInfo as Ask;
        use steamroom::messages::EMsg;
        // One round trip: Steam issues an ownership ticket for an app (or a
        // DLC) only to an account that owns it.
        if let Some(owned) = self.ownership_ticket(app).await? {
            return Ok(owned);
        }
        let known = self.licenses().clone();
        let ids = match known {
            Some(ids) => ids,
            None => {
                // Still queued if no other request has run on this
                // connection yet; if one has, it was dropped, and a fresh
                // connection (the actor's retry) gets it again.
                match tokio::time::timeout(
                    LICENSE_LIST_WAIT,
                    self.recv(crate::licenses::EMSG_LICENSE_LIST),
                )
                .await
                {
                    Ok(Ok(body)) => received_licences(&body)?,
                    Ok(Err(e)) => return Err(e),
                    Err(_) => {
                        return Err(CmFail::Connection(
                            "Steam's licence list for this session was missed".into(),
                        ));
                    }
                }
            }
        };
        let ids: Vec<steamroom::depot::PackageId> = ids
            .iter()
            .map(|p| steamroom::depot::PackageId(*p))
            .collect();
        let mut found = false;
        for chunk in ids.chunks(PACKAGES_PER_REQUEST) {
            let tokens = tokio::time::timeout(
                ROUND_TRIP,
                self.client.pics_get_package_access_tokens(chunk),
            )
            .await
            .map_err(|_| CmFail::Connection("PICS access tokens timed out".into()))?
            .map_err(refused_or_lost)?;
            let token = |p: u32| {
                tokens
                    .iter()
                    .find(|(id, _)| id.0 == p)
                    .map_or(0, |(_, t)| *t)
            };
            let req = Req {
                packages: chunk
                    .iter()
                    .map(|p| Ask {
                        packageid: Some(p.0),
                        access_token: Some(token(p.0)),
                    })
                    .collect(),
                meta_data_only: Some(false),
                ..Default::default()
            };
            let body = req.encode_to_vec();
            let m = self.msg(EMsg::CLIENT_PICS_PRODUCT_INFO_REQUEST.0, &body);
            self.client.send_msg(&m).await.map_err(refused_or_lost)?;
            loop {
                let body = tokio::time::timeout(
                    ROUND_TRIP,
                    self.recv(EMsg::CLIENT_PICS_PRODUCT_INFO_RESPONSE.0),
                )
                .await
                .map_err(|_| CmFail::Connection("PICS package info timed out".into()))??;
                // A bad part of a split answer leaves the rest queued:
                // drop the connection rather than let a later request read
                // them.
                let r = Resp::decode(&*body)
                    .map_err(|e| CmFail::Connection(format!("bad PICS package info: {e}")))?;
                for p in &r.packages {
                    let info = steamroom::apps::PackageInfo {
                        package_id: p.packageid.map(steamroom::depot::PackageId),
                        change_number: p.change_number,
                        kv_data: p.buffer.clone(),
                    };
                    match info.key_values() {
                        Ok(kv) => found |= crate::licenses::grants(&kv, app.0),
                        Err(e) => {
                            tracing::debug!(package = ?p.packageid, error = %e, "package info without key values")
                        }
                    }
                }
                if !r.response_pending.unwrap_or(false) {
                    break;
                }
            }
            if found {
                break;
            }
        }
        Ok(found)
    }
}

impl LoggedInConn {
    /// What an app ownership ticket request says about `app` (see
    /// [`crate::licenses::ownership_from_ticket`]).
    async fn ownership_ticket(&self, app: AppId) -> Result<Option<bool>, CmFail> {
        use crate::licenses::{
            EMSG_OWNERSHIP_TICKET, EMSG_OWNERSHIP_TICKET_RESPONSE, GetAppOwnershipTicket,
            ownership_from_ticket,
        };
        let body = GetAppOwnershipTicket {
            app_id: Some(app.0),
        }
        .encode_to_vec();
        let mut m = self.msg(EMSG_OWNERSHIP_TICKET, &body);
        m.header.jobid_source = Some(2);
        self.client.send_msg(&m).await.map_err(refused_or_lost)?;
        let answer = tokio::time::timeout(ROUND_TRIP, self.recv(EMSG_OWNERSHIP_TICKET_RESPONSE))
            .await
            .map_err(|_| CmFail::Connection("app ownership ticket timed out".into()))??;
        ownership_from_ticket(app.0, &answer).map_err(CmFail::Refused)
    }

    fn licenses(&self) -> std::sync::MutexGuard<'_, Option<Vec<u32>>> {
        self.licenses.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Read incoming messages (also inside `MULTI`) until one of type
    /// `emsg`, and return its body, keeping any licence list seen on the
    /// way. Everything else is dropped, as steamroom's own request loops do.
    async fn recv(&self, emsg: u32) -> Result<Vec<u8>, CmFail> {
        use steamroom::messages::EMsg;
        use steamroom::messages::header::PacketHeader;
        loop {
            let m = self.client.recv_msg().await.map_err(refused_or_lost)?;
            let msgs: Vec<(u32, Vec<u8>)> = if m.emsg == EMsg::MULTI {
                steamroom::client::multi::unpack_multi(&m.body)
                    .map_err(refused_or_lost)?
                    .iter()
                    .filter_map(|sub| match PacketHeader::parse(sub) {
                        Ok(PacketHeader::Protobuf { header, body }) => {
                            Some((header.emsg.0, body.to_vec()))
                        }
                        _ => None,
                    })
                    .collect()
            } else {
                vec![(m.emsg.0, m.body.to_vec())]
            };
            if let Some(body) = take(emsg, msgs, &self.licenses) {
                return Ok(body);
            }
        }
    }
}

/// The package ids of a licence list just received. A list that does not
/// parse drops the connection (the retry gets a fresh one) rather than
/// answering "owns nothing" for the rest of the process.
fn received_licences(body: &[u8]) -> Result<Vec<u32>, CmFail> {
    crate::licenses::package_ids(body)
        .map_err(|e| CmFail::Connection(format!("unreadable licence list: {e}")))
}

/// The body of the first message of type `emsg` in `msgs`, keeping the
/// package ids of any licence list among them in `licenses`.
fn take(
    emsg: u32,
    msgs: Vec<(u32, Vec<u8>)>,
    licenses: &std::sync::Mutex<Option<Vec<u32>>>,
) -> Option<Vec<u8>> {
    let mut found = None;
    for (e, body) in msgs {
        if e == crate::licenses::EMSG_LICENSE_LIST
            && let Ok(ids) = crate::licenses::package_ids(&body)
        {
            *licenses.lock().unwrap_or_else(|e| e.into_inner()) = Some(ids);
        }
        if e == emsg && found.is_none() {
            found = Some(body);
        }
    }
    found
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use steamroom::generated::CContentServerDirectoryServerInfo as Info;

    /// Scripted connections: connection `n` (0-based) behaves as `script[n]`.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Behave {
        Answer,
        Hang,
        Drop,
        Refuse,
    }

    #[derive(Default)]
    pub(crate) struct Counters {
        pub connects: AtomicUsize,
        pub calls: AtomicUsize,
        pub in_call: AtomicUsize,
        pub max_in_call: AtomicUsize,
    }

    pub(crate) struct FakeConnector {
        pub script: Vec<Behave>,
        pub cdn: Vec<CdnServer>,
        pub counters: Arc<Counters>,
        pub fail_connect: Option<fn() -> SteamError>,
    }

    pub(crate) struct FakeConn {
        behave: Behave,
        cdn: Vec<CdnServer>,
        counters: Arc<Counters>,
    }

    impl Connector for FakeConnector {
        type Conn = FakeConn;
        async fn connect(&self) -> Result<FakeConn, SteamError> {
            if let Some(f) = self.fail_connect {
                return Err(f());
            }
            let n = self.counters.connects.fetch_add(1, Ordering::SeqCst);
            Ok(FakeConn {
                behave: *self.script.get(n).unwrap_or(&Behave::Answer),
                cdn: self.cdn.clone(),
                counters: self.counters.clone(),
            })
        }
    }

    impl CmConn for FakeConn {
        async fn call(&self, rpc: &Rpc) -> Result<Reply, CmFail> {
            let c = &self.counters;
            c.calls.fetch_add(1, Ordering::SeqCst);
            let now = c.in_call.fetch_add(1, Ordering::SeqCst) + 1;
            c.max_in_call.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            let r = match self.behave {
                Behave::Answer => Ok(match rpc {
                    Rpc::DepotKey { depot, .. } => Reply::DepotKey(DepotKey([depot.0 as u8; 32])),
                    Rpc::ManifestCode { .. } => Reply::ManifestCode(77),
                    Rpc::CdnServers { .. } => Reply::CdnServers(self.cdn.clone()),
                    Rpc::CdnToken { .. } => Reply::CdnToken(Some("t".into())),
                    Rpc::BranchManifests { .. } => Reply::Manifests(vec![]),
                    Rpc::OwnsApp { app } => Reply::Owns(app.0 == 1746860),
                    Rpc::EncryptedAppTicket { app, .. } => Reply::Ticket(
                        crate::ticket::parse_response(
                            app.0,
                            &crate::ticket::tests::response(app.0, 1, true),
                        )
                        .unwrap(),
                    ),
                }),
                Behave::Hang => std::future::pending().await,
                Behave::Drop => Err(CmFail::Connection("socket closed".into())),
                Behave::Refuse => Err(CmFail::Refused(SteamError::AccessDenied(DepotId(1)))),
            };
            c.in_call.fetch_sub(1, Ordering::SeqCst);
            r
        }
        async fn heartbeat(&self) -> Result<(), CmFail> {
            Ok(())
        }
    }

    fn cfg() -> SessionConfig {
        SessionConfig {
            connect_timeout: Duration::from_millis(200),
            rpc_timeout: Duration::from_millis(100),
            heartbeat: Duration::from_millis(20),
            cell_id: 0,
        }
    }

    pub(crate) async fn fake(script: Vec<Behave>) -> (Cm, Arc<Counters>) {
        fake_with_cdn(script, vec![]).await
    }

    pub(crate) async fn fake_with_cdn(
        script: Vec<Behave>,
        cdn: Vec<CdnServer>,
    ) -> (Cm, Arc<Counters>) {
        let counters = Arc::new(Counters::default());
        let c = FakeConnector {
            script,
            cdn,
            counters: counters.clone(),
            fail_connect: None,
        };
        (Cm::start(c, cfg()).await.unwrap(), counters)
    }

    async fn fake_with_heartbeat(script: Vec<Behave>, heartbeat: Duration) -> (Cm, Arc<Counters>) {
        let counters = Arc::new(Counters::default());
        let c = FakeConnector {
            script,
            cdn: vec![],
            counters: counters.clone(),
            fail_connect: None,
        };
        let cfg = SessionConfig { heartbeat, ..cfg() };
        (Cm::start(c, cfg).await.unwrap(), counters)
    }

    fn key_rpc(d: u32) -> Rpc {
        Rpc::DepotKey {
            app: AppId(1),
            depot: DepotId(d),
        }
    }

    #[tokio::test]
    async fn concurrent_requests_are_serialized() {
        let (cm, n) = fake(vec![]).await;
        let calls = (0..10u32).map(|d| {
            let cm = cm.clone();
            tokio::spawn(async move { cm.call(key_rpc(d)).await })
        });
        for (d, h) in calls.enumerate() {
            match h.await.unwrap().unwrap() {
                Reply::DepotKey(k) => {
                    assert_eq!(k, DepotKey([d as u8; 32]), "reply routed to its caller")
                }
                r => panic!("{r:?}"),
            }
        }
        assert_eq!(n.max_in_call.load(Ordering::SeqCst), 1);
        assert_eq!(n.connects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn timeout_or_drop_reconnects_and_retries_once() {
        for bad in [Behave::Hang, Behave::Drop] {
            let (cm, n) = fake(vec![bad, Behave::Answer]).await;
            assert!(matches!(
                cm.call(key_rpc(3)).await.unwrap(),
                Reply::DepotKey(_)
            ));
            assert_eq!(n.connects.load(Ordering::SeqCst), 2, "{bad:?}");
        }
    }

    #[tokio::test]
    async fn gives_up_after_two_bad_connections() {
        let (cm, _) = fake(vec![Behave::Hang, Behave::Hang]).await;
        let err = cm.call(key_rpc(3)).await.unwrap_err();
        assert!(matches!(err, SteamError::Timeout { .. }), "{err}");
    }

    #[tokio::test]
    async fn refusals_are_not_retried() {
        let (cm, n) = fake(vec![Behave::Refuse]).await;
        assert!(matches!(
            cm.call(key_rpc(3)).await,
            Err(SteamError::AccessDenied(_))
        ));
        assert_eq!(n.calls.load(Ordering::SeqCst), 1);
        assert_eq!(n.connects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn connect_errors_surface_at_start() {
        let c = FakeConnector {
            script: vec![],
            cdn: vec![],
            counters: Arc::default(),
            fail_connect: Some(|| SteamError::LoginExpired {
                account: "alice".into(),
            }),
        };
        let err = Cm::start(c, cfg()).await.err().unwrap();
        assert!(err.to_string().contains("log in to Steam again"), "{err}");
    }

    fn info(ty: &str, host: &str, load: f32) -> Info {
        Info {
            r#type: Some(ty.into()),
            host: Some(host.into()),
            vhost: Some(host.into()),
            weighted_load: Some(load),
            https_support: Some("mandatory".into()),
            ..Default::default()
        }
    }

    #[test]
    fn selects_like_depotdownloader() {
        let mut restricted = info("CDN", "only-other-app", 1.0);
        restricted.allowed_app_ids = vec![1];
        let mut allowed = info("CDN", "lists-our-app", 5.0);
        allowed.allowed_app_ids = vec![1, 489830];
        let mut proxy = info("CDN", "proxy", 0.0);
        proxy.use_as_proxy = Some(true);
        let mut china = info("CDN", "china", 0.0);
        china.steam_china_only = Some(true);
        let mut plain_http = info("SteamCache", "isp-cache:8080", 3.0);
        plain_http.https_support = None;
        let infos = vec![
            info("CDN", "b", 2.0),
            info("OpenCache", "opencache", 0.0),
            restricted,
            allowed,
            proxy,
            china,
            plain_http,
            info("CDN", "a", 1.0),
            info("CDN", "a", 1.5), // same host twice
        ];
        let got = select_servers(&infos, AppId(489830));
        let hosts: Vec<_> = got.iter().map(|s| s.host.as_str()).collect();
        assert_eq!(hosts, ["a", "b", "isp-cache", "lists-our-app"]);
        assert_eq!((got[0].port, got[0].https), (443, true));
        assert_eq!((got[2].port, got[2].https), (80, false));
    }

    #[test]
    fn a_licence_list_is_kept_whatever_is_being_waited_for() {
        let l = std::sync::Mutex::new(None);
        let list = crate::licenses::tests::license_list(&[626104, 1]);
        // In the same MULTI as an unrelated answer, after it.
        let got = take(5527, vec![(5527, vec![7]), (780, list.clone())], &l);
        assert_eq!(got, Some(vec![7]));
        assert_eq!(*l.lock().unwrap(), Some(vec![1, 626104]));
        // Waited for itself.
        let l = std::sync::Mutex::new(None);
        assert_eq!(take(780, vec![(780, list.clone())], &l), Some(list));
        assert!(l.lock().unwrap().is_some());
        assert_eq!(take(5527, vec![(1, vec![])], &l), None);
    }

    #[test]
    fn an_unreadable_licence_list_drops_the_connection() {
        assert!(matches!(
            received_licences(&[0xff, 0xff]),
            Err(CmFail::Connection(_))
        ));
        let list = crate::licenses::tests::license_list(&[1]);
        assert!(matches!(received_licences(&list), Ok(v) if v == vec![1]));
    }

    #[test]
    fn ownership_gets_a_longer_deadline_than_one_request() {
        let c = SessionConfig::default();
        assert!(Rpc::OwnsApp { app: AppId(1) }.deadline(&c) > c.rpc_timeout);
        assert_eq!(key_rpc(1).deadline(&c), c.rpc_timeout);
    }

    #[test]
    fn zero_heartbeat_is_clamped_to_one_second() {
        assert_eq!(clamped_heartbeat(Duration::ZERO), Duration::from_secs(1));
        assert_eq!(
            clamped_heartbeat(Duration::from_millis(500)),
            Duration::from_millis(500)
        );
    }

    #[tokio::test]
    async fn a_zero_heartbeat_session_starts_and_stays_usable() {
        // Regression check for the actor task itself: before the clamp,
        // tokio::time::interval(Duration::ZERO) inside run() panicked,
        // which would silently end the task and every later call would
        // see "Steam session task stopped" instead of an answer.
        let (cm, _) = fake_with_heartbeat(vec![], Duration::ZERO).await;
        assert!(matches!(
            cm.call(key_rpc(1)).await.unwrap(),
            Reply::DepotKey(_)
        ));
    }

    #[test]
    fn default_connect_timeout_fits_the_directory_fetch_plus_three_cm_tries() {
        // connect_ready() budgets up to 10s per CM try, three tries, before
        // the directory fetch itself is even counted.
        let three_cm_tries = Duration::from_secs(10) * 3;
        assert!(SessionConfig::default().connect_timeout > three_cm_tries);
        assert_eq!(
            SessionConfig::default().connect_timeout,
            Duration::from_secs(45)
        );
    }

    #[test]
    fn cdn_server_directory_request_carries_the_session_cell_id() {
        let req = cdn_servers_request(7);
        assert_eq!(req.cell_id, Some(7));
        assert_eq!(req.max_servers, Some(30));
    }
}
