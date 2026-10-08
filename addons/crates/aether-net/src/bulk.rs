//! Bulk downloads for a full install: several HTTP clients, each with its
//! own connection pool (so its own HTTP/2 connection to a host), that
//! large range requests are spread over, and range bodies read as they
//! arrive instead of gathered whole.
//!
//! The file host serves each request at ~16–25 MiB/s after ~200 ms, and
//! the aggregate grows with requests in flight and with connections (one
//! connection tops out well below what several do). These clients go
//! around the per-host [`Limiter`](crate::Limiter): the caller (the
//! installer's scheduler) bounds what is in flight itself. No source events
//! are sent per chunk either: at hundreds of MiB/s they would swamp the
//! event channel; the caller counts bytes.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use reqwest::header::RANGE;
use reqwest::{Client, Response, StatusCode};
use url::Url;

use crate::error::{Result, SourceError, redact};
use crate::http::{HttpConfig, check, content_range};

/// How the bulk clients talk HTTP/2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BulkHttpConfig {
    /// Clients, and so connections to one host (at least one).
    pub conns: usize,
    /// HTTP/2 flow-control window of one stream, bytes.
    pub stream_window: u32,
    /// HTTP/2 flow-control window of one connection, bytes.
    pub conn_window: u32,
}

impl Default for BulkHttpConfig {
    fn default() -> BulkHttpConfig {
        BulkHttpConfig {
            conns: 8,
            stream_window: 8 << 20,
            conn_window: 64 << 20,
        }
    }
}

/// Several HTTP clients that bulk requests are spread over, round-robin.
/// Cheap to clone.
#[derive(Debug, Clone)]
pub struct BulkHttp {
    clients: Arc<Vec<Client>>,
    next: Arc<AtomicUsize>,
}

impl BulkHttp {
    /// `bulk.conns` clients with `http`'s user agent and timeouts.
    pub fn new(http: &HttpConfig, bulk: BulkHttpConfig) -> Result<BulkHttp> {
        let clients = (0..bulk.conns.max(1))
            .map(|_| {
                Client::builder()
                    .user_agent(http.user_agent.clone())
                    .connect_timeout(http.connect_timeout)
                    .read_timeout(http.read_timeout)
                    .http2_initial_stream_window_size(bulk.stream_window)
                    .http2_initial_connection_window_size(bulk.conn_window)
                    .http2_keep_alive_interval(Duration::from_secs(20))
                    .pool_idle_timeout(Duration::from_secs(90))
                    .build()
                    .map_err(|e| SourceError::Protocol {
                        url: String::new(),
                        msg: format!("cannot build HTTP client: {e}"),
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(BulkHttp {
            clients: Arc::new(clients),
            next: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// How many clients (connections per host) there are.
    pub fn conns(&self) -> usize {
        self.clients.len()
    }

    /// The client the next request goes over (round-robin).
    pub fn next_conn(&self) -> usize {
        self.next.fetch_add(1, Ordering::Relaxed) % self.clients.len()
    }

    /// One attempt at `GET url` for bytes `range` over client `conn`
    /// (modulo the number of clients): the response, once its status and
    /// headers say it is exactly that range of a `total`-byte file. Its
    /// body is read with [`RangeBody::chunk`].
    pub async fn get_range(
        &self,
        conn: usize,
        url: &Url,
        range: Range<u64>,
        total: u64,
    ) -> Result<RangeBody> {
        if range.start >= range.end || range.end > total {
            return Err(SourceError::Unsupported(format!(
                "range {range:?} of a {total}-byte file"
            )));
        }
        let client = &self.clients[conn % self.clients.len()];
        let resp = client
            .get(url.clone())
            .header(RANGE, format!("bytes={}-{}", range.start, range.end - 1))
            .send()
            .await
            .map_err(|e| SourceError::network(url, e))?;
        let resp = check(resp, url).await?;
        if resp.status() != StatusCode::PARTIAL_CONTENT {
            return Err(SourceError::protocol(
                url,
                format!(
                    "asked for a byte range, got HTTP {}",
                    resp.status().as_u16()
                ),
            ));
        }
        match content_range(&resp) {
            Some((_, _, have)) if have != total => {
                return Err(SourceError::archive_changed(url, have, total));
            }
            Some((start, end, _)) if start == range.start && end + 1 == range.end => {}
            other => {
                return Err(SourceError::protocol(
                    url,
                    format!("asked for bytes {range:?}, got Content-Range {other:?}"),
                ));
            }
        }
        Ok(RangeBody {
            resp,
            url: redact(url),
            left: range.end - range.start,
        })
    }
}

/// The body of a range response, read as it arrives. It must be exactly
/// the range asked for: a longer body is a protocol error, a shorter one a
/// [`SourceError::CorruptPart`].
#[derive(Debug)]
pub struct RangeBody {
    resp: Response,
    /// For errors: the URL without its query.
    url: String,
    /// Bytes still due.
    left: u64,
}

impl RangeBody {
    /// The next piece of the body, or `None` once all of it has arrived.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        let next = self.resp.chunk().await.map_err(|e| SourceError::Network {
            url: self.url.clone(),
            source: e.without_url(),
        })?;
        match next {
            Some(c) if c.len() as u64 > self.left => Err(SourceError::Protocol {
                url: self.url.clone(),
                msg: "body longer than the range asked for".into(),
            }),
            Some(c) => {
                self.left -= c.len() as u64;
                Ok(Some(c))
            }
            None if self.left > 0 => Err(SourceError::CorruptPart {
                what: self.url.clone(),
                msg: format!("body ended {} bytes early", self.left),
            }),
            None => Ok(None),
        }
    }

    /// Bytes of the body not yet read.
    pub fn left(&self) -> u64 {
        self.left
    }
}
