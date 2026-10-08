use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{CONTENT_RANGE, RANGE, RETRY_AFTER};
use reqwest::redirect::Policy;
use reqwest::{Client, Response, StatusCode};
use url::Url;

use crate::error::{Result, SourceError, redact, redact_raw};
use crate::events::{Events, Job};
use crate::limit::{Limiter, Permit};
use crate::retry::RetryPolicy;

/// Settings shared by every source.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Concurrent requests in total (spec §6 default 64).
    pub global_connections: usize,
    /// Concurrent requests to one host (spec §6 default 16).
    pub per_host_connections: usize,
    pub retry: RetryPolicy,
    pub connect_timeout: Duration,
    /// Longest silence while reading a response.
    pub read_timeout: Duration,
    pub user_agent: String,
    /// Range size for parallel whole-file HTTP downloads.
    pub chunk_size: u64,
    /// Ranges or CDN parts of one file fetched at once.
    pub parallel_parts: usize,
}

impl Default for HttpConfig {
    fn default() -> HttpConfig {
        HttpConfig {
            global_connections: 64,
            per_host_connections: 16,
            retry: RetryPolicy::default(),
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(60),
            user_agent: concat!("aether-net/", env!("CARGO_PKG_VERSION")).to_string(),
            chunk_size: 8 << 20,
            parallel_parts: 8,
        }
    }
}

/// The HTTP context every source shares: one connection pool, one limiter,
/// one retry policy, one event channel. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Http {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    client: Client,
    /// Never follows redirects: for requests carrying credentials (the
    /// Nexus `apikey` header), which must not reach another host.
    api_client: Client,
    limiter: Limiter,
    cfg: HttpConfig,
    events: Events,
}

impl Http {
    pub fn new(cfg: HttpConfig, events: Events) -> Result<Http> {
        let build = |redirect: Policy| {
            Client::builder()
                .user_agent(cfg.user_agent.clone())
                .connect_timeout(cfg.connect_timeout)
                .read_timeout(cfg.read_timeout)
                .redirect(redirect)
                .build()
                .map_err(|e| SourceError::Protocol {
                    url: String::new(),
                    msg: format!("cannot build HTTP client: {e}"),
                })
        };
        let client = build(Policy::default())?;
        let api_client = build(Policy::none())?;
        Ok(Http {
            inner: Arc::new(Inner {
                client,
                api_client,
                limiter: Limiter::new(cfg.global_connections, cfg.per_host_connections),
                cfg,
                events,
            }),
        })
    }

    pub fn events(&self) -> &Events {
        &self.inner.events
    }

    pub fn config(&self) -> &HttpConfig {
        &self.inner.cfg
    }

    /// `GET url` for a small document (a JSON index, a gallery list): the
    /// whole body, at most `max_body` bytes, with the limiter and retry
    /// policy of every other request. Redirects are followed; errors name
    /// the URL without its query.
    pub async fn get_bytes(&self, url: &str, max_body: u64) -> Result<Vec<u8>> {
        let url = Url::parse(url).map_err(|e| SourceError::Protocol {
            url: redact_raw(url),
            msg: format!("not a URL: {e}"),
        })?;
        let job = self.start(format!("get {}", redact(&url)), None);
        let r = self
            .inner
            .cfg
            .retry
            .run(&job, || async {
                let _permit = self.permit(&url).await;
                let resp = self
                    .client()
                    .get(url.clone())
                    .send()
                    .await
                    .map_err(|e| SourceError::network(&url, e))?;
                read_body_max(check(resp, &url).await?, &url, &job, max_body).await
            })
            .await;
        job.complete(r)
    }

    #[doc(hidden)]
    pub fn client(&self) -> &Client {
        &self.inner.client
    }

    /// Like [`Http::client`], but never follows a redirect.
    #[doc(hidden)]
    pub fn api_client(&self) -> &Client {
        &self.inner.api_client
    }

    #[doc(hidden)]
    pub fn start(&self, label: impl Into<String>, total: Option<u64>) -> Job {
        self.inner.events.start(label, total)
    }

    #[doc(hidden)]
    pub async fn permit(&self, url: &Url) -> Permit {
        self.inner
            .limiter
            .acquire(url.host_str().unwrap_or(""))
            .await
    }

    /// One attempt at `GET url` for bytes `range`. The server must answer
    /// 206 with exactly that range and, with `total`, report a file of
    /// exactly that many bytes (a changed file is not read from).
    #[doc(hidden)]
    pub async fn get_range(
        &self,
        url: &Url,
        range: Range<u64>,
        total: Option<u64>,
        job: &Job,
    ) -> Result<Vec<u8>> {
        self.get_range_with(url, range, total, job, &[]).await
    }

    /// [`get_range`](Self::get_range) with extra request headers.
    #[doc(hidden)]
    pub async fn get_range_with(
        &self,
        url: &Url,
        range: Range<u64>,
        total: Option<u64>,
        job: &Job,
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>> {
        if range.start >= range.end {
            return Ok(Vec::new());
        }
        let _permit = self.permit(url).await;
        let mut req = self
            .client()
            .get(url.clone())
            .header(RANGE, format!("bytes={}-{}", range.start, range.end - 1));
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.send().await.map_err(|e| SourceError::network(url, e))?;
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
        let got = content_range(&resp);
        if let (Some(want), Some((_, _, have))) = (total, got)
            && want != have
        {
            return Err(SourceError::archive_changed(url, have, want));
        }
        match got {
            Some((start, end, _)) if start == range.start && end + 1 == range.end => {}
            other => {
                return Err(SourceError::protocol(
                    url,
                    format!("asked for bytes {range:?}, got Content-Range {other:?}"),
                ));
            }
        }
        read_body(resp, url, job, Some(range.end - range.start)).await
    }

    /// One attempt at `GET url` for its last `n` bytes (a suffix range,
    /// `bytes=-n`): the bytes, and the file's total length from the
    /// response's `Content-Range`, so a caller that reads a file's tail
    /// needs no request to learn its length first. A file shorter than `n`
    /// comes whole, as a 206 over all of it or, from a server that ignores
    /// Range, as a 200 of at most `n` bytes; a longer 200 is an error.
    #[doc(hidden)]
    pub async fn get_suffix(&self, url: &Url, n: u64, job: &Job) -> Result<(Vec<u8>, u64)> {
        let _permit = self.permit(url).await;
        let resp = self
            .client()
            .get(url.clone())
            .header(RANGE, format!("bytes=-{n}"))
            .send()
            .await
            .map_err(|e| SourceError::network(url, e))?;
        let resp = check(resp, url).await?;
        if resp.status() != StatusCode::PARTIAL_CONTENT {
            let body = read_body_max(resp, url, job, n)
                .await
                .map_err(|e| match e {
                    SourceError::Protocol { url, .. } => SourceError::Protocol {
                        url,
                        msg: "file host ignored the Range header".into(),
                    },
                    e => e,
                })?;
            let total = body.len() as u64;
            return Ok((body, total));
        }
        match content_range(&resp) {
            Some((start, end, total))
                if end.checked_add(1) == Some(total) && start == total.saturating_sub(n) =>
            {
                let body = read_body(resp, url, job, Some(total - start)).await?;
                Ok((body, total))
            }
            other => Err(SourceError::protocol(
                url,
                format!("asked for the last {n} bytes, got Content-Range {other:?}"),
            )),
        }
    }

    /// One attempt at learning the size of `url` with a one-byte range
    /// request (HEAD is not reliable: Nexus's file host omits
    /// Content-Length). `None` means the server ignores Range.
    #[doc(hidden)]
    pub async fn range_len(&self, url: &Url) -> Result<Option<u64>> {
        let _permit = self.permit(url).await;
        let resp = self
            .client()
            .get(url.clone())
            .header(RANGE, "bytes=0-0")
            .send()
            .await
            .map_err(|e| SourceError::network(url, e))?;
        if resp.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            return Ok(Some(0)); // an empty file has no byte 0
        }
        let resp = check(resp, url).await?;
        match resp.status() {
            StatusCode::PARTIAL_CONTENT => match content_range(&resp) {
                Some((0, 0, total)) => Ok(Some(total)),
                other => Err(SourceError::protocol(
                    url,
                    format!("bad Content-Range for bytes 0-0: {other:?}"),
                )),
            },
            _ => Ok(None),
        }
    }
}

/// Turn an error status into a [`SourceError`]; pass success through.
#[doc(hidden)]
pub async fn check(resp: Response, url: &Url) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let retry_after = resp
        .headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let body = error_body(resp, ERROR_BODY_MAX).await;
    Err(match status.as_u16() {
        404 => SourceError::NotFound { what: redact(url) },
        429 => SourceError::RateLimited {
            host: url.host_str().unwrap_or("").to_string(),
            retry_after,
        },
        s => SourceError::Status {
            url: redact(url),
            status: s,
            body,
        },
    })
}

/// Bytes of an error response's body kept for the error message.
#[doc(hidden)]
pub const ERROR_BODY_MAX: usize = 300;

/// At most `max` bytes (cut at a UTF-8 boundary) of an error response's
/// body. Reads only as many chunks as needed, then drops the response, so a
/// huge or never-ending error body costs nothing.
#[doc(hidden)]
pub async fn error_body(mut resp: Response, max: usize) -> String {
    // A few extra bytes so a character cut at `max` can still be completed.
    let want = max + 3;
    let mut buf = Vec::with_capacity(want.min(4096));
    while buf.len() < want {
        match resp.chunk().await {
            Ok(Some(c)) => buf.extend_from_slice(&c[..c.len().min(want - buf.len())]),
            _ => break,
        }
    }
    drop(resp);
    let mut body = match String::from_utf8(buf) {
        Ok(s) => s,
        Err(e) => {
            let valid = e.utf8_error().valid_up_to();
            let mut b = e.into_bytes();
            if b.len() - valid < 4 {
                // Only a character cut short at the end.
                b.truncate(valid);
            }
            String::from_utf8_lossy(&b).into_owned()
        }
    };
    body.truncate(body.floor_char_boundary(max));
    body
}

/// Read a whole response body, reporting progress. With `expect`, the
/// body must be exactly that long.
#[doc(hidden)]
pub async fn read_body(
    resp: Response,
    url: &Url,
    job: &Job,
    expect: Option<u64>,
) -> Result<Vec<u8>> {
    read_body_inner(resp, url, job, expect, expect).await
}

/// Read a whole response body of at most `max` bytes, reporting progress.
#[doc(hidden)]
pub async fn read_body_max(resp: Response, url: &Url, job: &Job, max: u64) -> Result<Vec<u8>> {
    read_body_inner(resp, url, job, None, Some(max)).await
}

async fn read_body_inner(
    mut resp: Response,
    url: &Url,
    job: &Job,
    expect: Option<u64>,
    max: Option<u64>,
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(expect.unwrap_or(0).min(64 << 20) as usize);
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| SourceError::network(url, e))?
    {
        if let Some(n) = max
            && (out.len() + chunk.len()) as u64 > n
        {
            let what = if expect.is_some() {
                "expected"
            } else {
                "allowed"
            };
            return Err(SourceError::protocol(
                url,
                format!("body longer than the {what} {n} bytes"),
            ));
        }
        job.progress(chunk.len() as u64);
        out.extend_from_slice(&chunk);
    }
    if let Some(n) = expect
        && out.len() as u64 != n
    {
        return Err(SourceError::CorruptPart {
            what: redact(url),
            msg: format!("body ended after {} of {n} bytes", out.len()),
        });
    }
    Ok(out)
}

/// `Content-Range: bytes a-b/total` -> (a, b, total).
#[doc(hidden)]
pub fn content_range(resp: &Response) -> Option<(u64, u64, u64)> {
    let v = resp.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    let rest = v.trim().strip_prefix("bytes ")?;
    let (span, total) = rest.split_once('/')?;
    let (a, b) = span.split_once('-')?;
    Some((a.parse().ok()?, b.parse().ok()?, total.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Response {
        let mut b = http::Response::builder().status(status);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        Response::from(b.body(body.to_vec()).unwrap())
    }

    fn url() -> Url {
        Url::parse("https://files.example/x.zip?sig=SECRET").unwrap()
    }

    #[test]
    fn parses_content_range() {
        let r = resp(206, &[("content-range", "bytes 0-15/129552")], b"");
        assert_eq!(content_range(&r), Some((0, 15, 129_552)));
        let r = resp(206, &[("content-range", "bytes */10")], b"");
        assert_eq!(content_range(&r), None);
        assert_eq!(content_range(&resp(200, &[], b"")), None);
    }

    #[tokio::test]
    async fn check_maps_statuses() {
        assert!(check(resp(206, &[], b""), &url()).await.is_ok());
        let e = check(resp(404, &[], b"nope"), &url()).await.unwrap_err();
        assert!(
            matches!(&e, SourceError::NotFound { what } if what == "https://files.example/x.zip")
        );
        let e = check(resp(429, &[("retry-after", "7")], b""), &url())
            .await
            .unwrap_err();
        assert_eq!(e.retry_after(), Some(Duration::from_secs(7)));
        let long = "é".repeat(400);
        let e = check(resp(503, &[], long.as_bytes()), &url())
            .await
            .unwrap_err();
        assert!(e.is_retryable());
        assert!(matches!(&e, SourceError::Status { body, .. } if body.len() <= 300));
        assert!(!e.to_string().contains("SECRET"));
    }

    #[tokio::test]
    async fn read_body_checks_the_length() {
        let job = Events::default().start("t", None);
        let ok = read_body(resp(200, &[], b"abcd"), &url(), &job, Some(4)).await;
        assert_eq!(ok.unwrap(), b"abcd");
        let short = read_body(resp(200, &[], b"abc"), &url(), &job, Some(4)).await;
        assert!(matches!(short, Err(SourceError::CorruptPart { .. })));
        let long = read_body(resp(200, &[], b"abcde"), &url(), &job, Some(4)).await;
        assert!(matches!(long, Err(SourceError::Protocol { .. })));
        let _ = job.complete(Ok(()));
    }

    #[tokio::test]
    async fn read_body_max_refuses_a_body_over_the_cap() {
        let job = Events::default().start("t", None);
        let ok = read_body_max(resp(200, &[], b"abcd"), &url(), &job, 4).await;
        assert_eq!(ok.unwrap(), b"abcd");
        let long = read_body_max(resp(200, &[], b"abcde"), &url(), &job, 4).await;
        assert!(matches!(long, Err(SourceError::Protocol { .. })));
        let _ = job.complete(Ok(()));
    }

    #[tokio::test]
    async fn error_body_is_read_only_up_to_the_cap() {
        let body = error_body(resp(500, &[], &[b'x'; 10_000]), 300).await;
        assert_eq!(body.len(), 300);
        let body = error_body(resp(500, &[], "é".repeat(400).as_bytes()), 301).await;
        assert!(
            body.len() <= 301 && body.chars().all(|c| c == 'é'),
            "{body}"
        );
    }
}
