//! Test fixtures built the way Steam builds them: CDN manifest bodies,
//! encrypted chunks, and an in-process fake CDN.
use crate::ids::{ChunkId, DepotId, DepotKey, ManifestId};
use crate::manifest::ChunkRef;
use prost::Message;
use steamroom::generated::content_manifest_payload::{FileMapping, file_mapping::ChunkData};
use steamroom::generated::{ContentManifestMetadata, ContentManifestPayload};

pub(crate) const KEY: DepotKey = DepotKey([0x42; 32]);

pub(crate) fn steam_adler(data: &[u8]) -> u32 {
    steamroom::util::checksum::SteamAdler32::compute(data).0
}

pub(crate) fn chunk_ref_for(data: &[u8], offset: u64) -> ChunkRef {
    ChunkRef {
        id: ChunkId(sha1_smol::Sha1::from(data).digest().bytes()),
        offset,
        len: data.len() as u32,
        adler: steam_adler(data),
    }
}

/// A file for [`manifest_body`]: path, content, chunk size.
pub(crate) struct FixtureFile<'a> {
    pub path: &'a str,
    pub data: &'a [u8],
    pub chunk: usize,
}

/// Split `data` into chunks of `size` bytes (the last may be shorter).
pub(crate) fn split(data: &[u8], size: usize) -> Vec<(ChunkRef, Vec<u8>)> {
    data.chunks(size)
        .enumerate()
        .map(|(i, c)| (chunk_ref_for(c, (i * size) as u64), c.to_vec()))
        .collect()
}

/// A manifest body as the CDN serves it: a one-entry zip of the v5 binary
/// manifest (payload, metadata, end marker). File names are stored in the
/// clear, or encrypted with `names_key` the way Steam does; `dirs` adds
/// directory entries, which the parser must skip.
pub(crate) fn manifest_body(
    depot: DepotId,
    id: ManifestId,
    files: &[FixtureFile],
    dirs: &[&str],
    names_key: Option<&DepotKey>,
) -> Vec<u8> {
    let name = |n: &str| match names_key {
        None => n.to_string(),
        Some(k) => encrypt_name(n, k),
    };
    let mut mappings: Vec<FileMapping> = files
        .iter()
        .map(|f| FileMapping {
            filename: Some(name(f.path)),
            size: Some(f.data.len() as u64),
            flags: Some(0),
            sha_content: Some(sha1_smol::Sha1::from(f.data).digest().bytes().to_vec()),
            // Steam does not promise chunk order; store them reversed.
            chunks: split(f.data, f.chunk)
                .into_iter()
                .rev()
                .map(|(c, _)| ChunkData {
                    sha: Some(c.id.0.to_vec()),
                    crc: Some(c.adler),
                    offset: Some(c.offset),
                    cb_original: Some(c.len),
                    cb_compressed: Some(c.len),
                })
                .collect(),
            ..Default::default()
        })
        .collect();
    for d in dirs {
        mappings.push(FileMapping {
            filename: Some(name(d)),
            size: Some(0),
            flags: Some(0x40),
            ..Default::default()
        });
    }
    let payload = ContentManifestPayload { mappings }.encode_to_vec();
    let meta = ContentManifestMetadata {
        depot_id: Some(depot.0),
        gid_manifest: Some(id.0),
        filenames_encrypted: Some(names_key.is_some()),
        ..Default::default()
    }
    .encode_to_vec();
    let mut bin = Vec::new();
    for (magic, body) in [(0x71F6_17D0u32, &payload), (0x1F48_12BE, &meta)] {
        bin.extend_from_slice(&magic.to_le_bytes());
        bin.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bin.extend_from_slice(body);
    }
    bin.extend_from_slice(&0x32C4_15ABu32.to_le_bytes());
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.start_file("z", zip::write::SimpleFileOptions::default())
        .unwrap();
    std::io::Write::write_all(&mut zip, &bin).unwrap();
    zip.finish().unwrap().into_inner()
}

/// A manifest body like [`manifest_body`], but with the metadata section
/// dropped entirely — as if the CDN served a corrupt response carrying no
/// depot id or manifest id. `DepotManifest::from_cdn_bytes` must reject
/// anything built this way rather than trust the caller's requested ids.
pub(crate) fn manifest_body_no_metadata(files: &[FixtureFile]) -> Vec<u8> {
    let mappings: Vec<FileMapping> = files
        .iter()
        .map(|f| FileMapping {
            filename: Some(f.path.to_string()),
            size: Some(f.data.len() as u64),
            flags: Some(0),
            sha_content: Some(sha1_smol::Sha1::from(f.data).digest().bytes().to_vec()),
            chunks: split(f.data, f.chunk)
                .into_iter()
                .map(|(c, _)| ChunkData {
                    sha: Some(c.id.0.to_vec()),
                    crc: Some(c.adler),
                    offset: Some(c.offset),
                    cb_original: Some(c.len),
                    cb_compressed: Some(c.len),
                })
                .collect(),
            ..Default::default()
        })
        .collect();
    let payload = ContentManifestPayload { mappings }.encode_to_vec();
    let mut bin = Vec::new();
    bin.extend_from_slice(&0x71F6_17D0u32.to_le_bytes());
    bin.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bin.extend_from_slice(&payload);
    bin.extend_from_slice(&0x32C4_15ABu32.to_le_bytes());
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.start_file("z", zip::write::SimpleFileOptions::default())
        .unwrap();
    std::io::Write::write_all(&mut zip, &bin).unwrap();
    zip.finish().unwrap().into_inner()
}

/// Steam's file-name encryption: base64(ECB(IV) || CBC(name + NUL)).
fn encrypt_name(name: &str, key: &DepotKey) -> String {
    use base64::Engine;
    let iv = [0x29u8; 16];
    let mut plain = name.as_bytes().to_vec();
    plain.push(0);
    let mut out = steamroom::crypto::symmetric_encrypt_ecb_nopad(&iv, &key.0).unwrap();
    out.extend(steamroom::crypto::symmetric_encrypt_cbc(&plain, &key.0, &iv).unwrap());
    base64::engine::general_purpose::STANDARD.encode(out)
}

// ---- chunk and CDN fixtures ----

/// VSZa-wrap (Valve zstd), then AES-256: ECB-encrypted IV, CBC payload.
pub(crate) fn encrypt_chunk(data: &[u8], key: &DepotKey) -> Vec<u8> {
    let z = zstd::bulk::compress(data, 3).unwrap();
    let mut vsza = b"VSZa".to_vec();
    vsza.extend_from_slice(&[0; 4]);
    vsza.extend_from_slice(&z);
    vsza.extend_from_slice(&[0; 4]);
    vsza.extend_from_slice(&(data.len() as u64).to_le_bytes());
    vsza.extend_from_slice(b"zsv");
    let iv = [0x17u8; 16];
    let mut out = steamroom::crypto::symmetric_encrypt_ecb_nopad(&iv, &key.0).unwrap();
    out.extend(steamroom::crypto::symmetric_encrypt_cbc(&vsza, &key.0, &iv).unwrap());
    out
}

/// How a [`FakeCdn`] answers.
#[derive(Clone, Debug)]
pub(crate) enum Mode {
    Ok,
    Status(u16),
    /// Serve the body with one byte flipped.
    Corrupt,
    /// Accept the request and never answer.
    Hang,
    /// Promise the full body, send half, close the connection.
    Truncate,
    /// 403 unless the query string is exactly this.
    RequireToken(String),
}

#[derive(Default)]
struct FakeState {
    bodies: std::collections::HashMap<String, Vec<u8>>,
    log: Vec<String>,
    connections: usize,
    in_flight: usize,
    max_in_flight: usize,
}

/// A minimal HTTP/1.1 CDN on 127.0.0.1 serving registered paths.
pub(crate) struct FakeCdn {
    port: u16,
    state: std::sync::Arc<std::sync::Mutex<FakeState>>,
    mode: std::sync::Arc<std::sync::Mutex<(Mode, std::time::Duration)>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeCdn {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeCdn {
    /// A CDN that closes the connection after every response.
    pub(crate) async fn start() -> FakeCdn {
        FakeCdn::start_with(false).await
    }

    /// A CDN that keeps a connection open for more requests, as real ones
    /// do: [`connections`](Self::connections) then tells whether a client
    /// reused its connection.
    pub(crate) async fn start_keep_alive() -> FakeCdn {
        FakeCdn::start_with(true).await
    }

    async fn start_with(keep_alive: bool) -> FakeCdn {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = std::sync::Arc::new(std::sync::Mutex::new(FakeState::default()));
        let mode =
            std::sync::Arc::new(std::sync::Mutex::new((Mode::Ok, std::time::Duration::ZERO)));
        let (st, md) = (state.clone(), mode.clone());
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let (st, md) = (st.clone(), md.clone());
                st.lock().unwrap().connections += 1;
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 1024];
                    loop {
                        req.clear();
                        while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                            match sock.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => req.extend_from_slice(&buf[..n]),
                            }
                        }
                        let line = String::from_utf8_lossy(&req)
                            .lines()
                            .next()
                            .unwrap_or("")
                            .to_string();
                        let target = line.split(' ').nth(1).unwrap_or("").to_string();
                        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                        let (mode, delay) = md.lock().unwrap().clone();
                        {
                            let mut s = st.lock().unwrap();
                            s.log.push(target.clone());
                            s.in_flight += 1;
                            s.max_in_flight = s.max_in_flight.max(s.in_flight);
                        }
                        tokio::time::sleep(delay).await;
                        let body = st.lock().unwrap().bodies.get(path).cloned();
                        let (code, body) = match (mode, body) {
                            (Mode::Hang, _) => {
                                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                                return;
                            }
                            (Mode::Status(c), _) => (c, Vec::new()),
                            (Mode::RequireToken(t), Some(b)) if query == t => (200, b),
                            (Mode::RequireToken(_), _) => (403, Vec::new()),
                            (_, None) => (404, Vec::new()),
                            (Mode::Corrupt, Some(mut b)) => {
                                let last = b.len() - 1;
                                b[last] ^= 0xFF;
                                (200, b)
                            }
                            (Mode::Truncate, Some(b)) => {
                                let head = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    b.len()
                                );
                                let _ = sock.write_all(head.as_bytes()).await;
                                let _ = sock.write_all(&b[..b.len() / 2]).await;
                                st.lock().unwrap().in_flight -= 1;
                                return;
                            }
                            (Mode::Ok, Some(b)) => (200, b),
                        };
                        st.lock().unwrap().in_flight -= 1;
                        let connection = if keep_alive { "keep-alive" } else { "close" };
                        let head = format!(
                            "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n",
                            body.len()
                        );
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.write_all(&body).await;
                        if !keep_alive {
                            let _ = sock.shutdown().await;
                            return;
                        }
                    }
                });
            }
        });
        FakeCdn {
            port,
            state,
            mode,
            task,
        }
    }

    pub(crate) fn server(&self) -> crate::cdn::CdnServer {
        crate::cdn::CdnServer {
            host: "127.0.0.1".into(),
            port: self.port,
            https: false,
        }
    }

    pub(crate) fn put(&self, path: &str, body: Vec<u8>) {
        self.state
            .lock()
            .unwrap()
            .bodies
            .insert(path.to_string(), body);
    }

    /// Serve every chunk of `parts` (from [`split`]) encrypted with `key`.
    pub(crate) fn put_chunks(&self, depot: DepotId, parts: &[(ChunkRef, Vec<u8>)], key: &DepotKey) {
        for (c, plain) in parts {
            self.put(
                &format!("/depot/{depot}/chunk/{}", c.id),
                encrypt_chunk(plain, key),
            );
        }
    }

    pub(crate) fn set_mode(&self, mode: Mode) {
        self.mode.lock().unwrap().0 = mode;
    }

    pub(crate) fn set_delay(&self, d: std::time::Duration) {
        self.mode.lock().unwrap().1 = d;
    }

    pub(crate) fn log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }

    pub(crate) fn clear_log(&self) {
        self.state.lock().unwrap().log.clear();
    }

    pub(crate) fn max_in_flight(&self) -> usize {
        self.state.lock().unwrap().max_in_flight
    }

    /// Connections accepted so far.
    pub(crate) fn connections(&self) -> usize {
        self.state.lock().unwrap().connections
    }
}

/// What a [`FakeProbe`] says of one host.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Probed {
    Takes(std::time::Duration),
    Fails,
    /// Never answers: the pool's probe timeout has to end it.
    Hangs,
}

/// A connect probe that asks a function instead of the network.
pub(crate) struct FakeProbe<F>(pub F);

impl<F> crate::cdn::Probe for FakeProbe<F>
where
    F: Fn(&crate::cdn::CdnServer) -> Probed + Send + Sync,
{
    fn connect<'a>(
        &'a self,
        server: &'a crate::cdn::CdnServer,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::io::Result<std::time::Duration>> + Send + 'a>,
    > {
        let answer = (self.0)(server);
        Box::pin(async move {
            match answer {
                Probed::Takes(d) => Ok(d),
                Probed::Fails => Err(std::io::Error::other("connection refused")),
                Probed::Hangs => std::future::pending().await,
            }
        })
    }
}

/// A pre-connect that records the port of each host it is asked for
/// instead of connecting.
#[derive(Default)]
pub(crate) struct FakePreconnect(pub std::sync::Mutex<Vec<u16>>);

impl crate::cdn::Preconnect for FakePreconnect {
    fn connect<'a>(
        &'a self,
        server: &'a crate::cdn::CdnServer,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        self.0.lock().unwrap().push(server.port);
        Box::pin(async { true })
    }
}
