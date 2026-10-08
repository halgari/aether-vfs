use std::sync::Arc;

use aether_archive::Xxh64;
use futures_util::stream::{self, StreamExt};
use reqwest::StatusCode;
use url::Url;
use xxhash_rust::xxh64::Xxh64 as Hasher;

use crate::error::{Result, SourceError, redact, redact_raw};
use crate::events::Job;
use crate::http::{Http, check};
use crate::sink::{BlobSink, write_blocking};

/// Size and xxHash64 of a completed download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Downloaded {
    pub len: u64,
    pub hash: Xxh64,
}

/// A file on a plain HTTP(S) server (GitHub releases, skse.silverlock.org).
/// Redirects are followed. Servers that honour Range are downloaded in
/// parallel ranges; others as one stream, restarted from zero on failure.
#[derive(Debug, Clone)]
pub struct HttpFile {
    http: Http,
    url: Url,
}

const STREAM_WRITE: usize = 1 << 20;

impl HttpFile {
    pub fn new(http: Http, url: &str) -> Result<HttpFile> {
        let parsed = Url::parse(url).map_err(|e| SourceError::Protocol {
            url: redact_raw(url),
            msg: format!("not a URL: {e}"),
        })?;
        Ok(HttpFile { http, url: parsed })
    }

    /// Download the whole file into `sink`, checking it against `expected`
    /// (hash) and `expected_len` (size, e.g. the modlist's archive size). A
    /// server whose size disagrees with `expected_len` is refused before
    /// any data is fetched.
    pub async fn download(
        &self,
        sink: Arc<dyn BlobSink>,
        expected: Option<Xxh64>,
        expected_len: Option<u64>,
    ) -> Result<Downloaded> {
        let job = self
            .http
            .start(format!("download {}", redact(&self.url)), expected_len);
        let r = self
            .download_inner(&sink, expected, expected_len, &job)
            .await;
        job.complete(r)
    }

    async fn download_inner(
        &self,
        sink: &Arc<dyn BlobSink>,
        expected: Option<Xxh64>,
        expected_len: Option<u64>,
        job: &Job,
    ) -> Result<Downloaded> {
        let retry = &self.http.config().retry;
        let len = retry.run(job, || self.http.range_len(&self.url)).await?;
        let got = match len {
            Some(len) => {
                self.check_len(len, expected_len)?;
                self.ranged(sink, len, job).await?
            }
            None => {
                retry
                    .run(job, || self.single(sink, expected_len, job))
                    .await?
            }
        };
        if let Some(want) = expected
            && want != got.hash
        {
            return Err(SourceError::HashMismatch {
                what: redact(&self.url),
                expected: want,
                actual: got.hash,
            });
        }
        Ok(got)
    }

    /// The server's size must match the caller's, when both are known.
    fn check_len(&self, server: u64, expected_len: Option<u64>) -> Result<()> {
        match expected_len {
            Some(want) if want != server => Err(SourceError::protocol(
                &self.url,
                format!("server has {server} bytes, expected {want}"),
            )),
            _ => Ok(()),
        }
    }

    async fn ranged(&self, sink: &Arc<dyn BlobSink>, len: u64, job: &Job) -> Result<Downloaded> {
        let cfg = self.http.config();
        let chunk = cfg.chunk_size.max(1);
        let ranges = (0..len.div_ceil(chunk)).map(|i| i * chunk..((i + 1) * chunk).min(len));
        // `buffered` runs up to `parallel_parts` ranges at once but yields
        // them in order, so the hash is computed as the bytes stream in.
        let mut parts = stream::iter(ranges)
            .map(|r| async move {
                let data = cfg
                    .retry
                    .run(job, || self.http.get_range(&self.url, r.clone(), None, job))
                    .await?;
                Ok::<_, SourceError>((r.start, data))
            })
            .buffered(cfg.parallel_parts.max(1));
        let mut hasher = Hasher::new(0);
        while let Some(part) = parts.next().await {
            let (off, data) = part?;
            hasher.update(&data);
            write_blocking(sink, off, data).await?;
        }
        Ok(Downloaded {
            len,
            hash: Xxh64(hasher.digest()),
        })
    }

    async fn single(
        &self,
        sink: &Arc<dyn BlobSink>,
        expected_len: Option<u64>,
        job: &Job,
    ) -> Result<Downloaded> {
        let _permit = self.http.permit(&self.url).await;
        let resp = self
            .http
            .client()
            .get(self.url.clone())
            .send()
            .await
            .map_err(|e| SourceError::network(&self.url, e))?;
        let mut resp = check(resp, &self.url).await?;
        if resp.status() != StatusCode::OK {
            return Err(SourceError::protocol(
                &self.url,
                format!("expected HTTP 200, got {}", resp.status().as_u16()),
            ));
        }
        let declared = resp.content_length();
        if let Some(d) = declared {
            self.check_len(d, expected_len)?;
        }
        let mut hasher = Hasher::new(0);
        let (mut off, mut buf) = (0u64, Vec::with_capacity(STREAM_WRITE));
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| SourceError::network(&self.url, e))?
        {
            if let Some(want) = expected_len
                && off + (buf.len() + chunk.len()) as u64 > want
            {
                return Err(SourceError::protocol(
                    &self.url,
                    format!("body longer than the expected {want} bytes"),
                ));
            }
            job.progress(chunk.len() as u64);
            hasher.update(&chunk);
            buf.extend_from_slice(&chunk);
            if buf.len() >= STREAM_WRITE {
                let n = buf.len() as u64;
                write_blocking(sink, off, std::mem::take(&mut buf)).await?;
                off += n;
            }
        }
        let n = buf.len() as u64;
        write_blocking(sink, off, buf).await?;
        off += n;
        if let Some(d) = declared
            && d != off
        {
            return Err(SourceError::CorruptPart {
                what: redact(&self.url),
                msg: format!("body ended after {off} of {d} bytes"),
            });
        }
        if let Some(want) = expected_len
            && want != off
        {
            return Err(SourceError::CorruptPart {
                what: redact(&self.url),
                msg: format!("body ended after {off} of {want} bytes"),
            });
        }
        Ok(Downloaded {
            len: off,
            hash: Xxh64(hasher.digest()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Events;
    use crate::http::HttpConfig;

    #[test]
    fn a_malformed_url_does_not_leak_its_query_in_the_error() {
        let http = Http::new(HttpConfig::default(), Events::default()).unwrap();
        let e = HttpFile::new(http, "ht!tp://host/p?sig=SECRET").unwrap_err();
        assert!(!e.to_string().contains("SECRET"), "{e}");
    }
}
