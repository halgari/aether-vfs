use std::future::Future;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use aether_archive::seekable::{FOOTER_LEN, FrameEntry, SeekTable, decompress_frame_into};
use aether_archive::zip::{
    CentralDirectory, EndRecords, LOCAL_HEADER_LEN, MAX_TAIL, METHOD_STORED, METHOD_ZSTD,
    SeekableEntry, ZIP64_EOCD_LEN, ZipEntry, ZipIndex, local_data_range, locate_central_directory,
};
use futures_util::stream::{self, StreamExt};
use tokio::sync::watch;
use tokio::task::AbortHandle;
use url::Url;

use super::NexusClient;
use aether_net::error::{Result, SourceError};
use aether_net::events::Job;

/// Bytes read from the end of the zip on open. Covers the end records and,
/// for most archives, the whole central directory.
const TAIL_READ: u64 = 256 << 10;
/// A central directory larger than this is refused.
const MAX_CENTRAL_DIRECTORY: u64 = 256 << 20;
/// Adjacent frames are fetched in one request up to this many compressed bytes.
const MAX_REQUEST: u64 = 32 << 20;
/// Extra local-header bytes fetched speculatively with a whole entry
/// (Nexus writes 56: ZIP64 + NTFS).
const LOCAL_EXTRA_GUESS: u64 = 256;
/// A first read of an entry whose bytes start at most this far into the
/// entry's data fetches the local header in the same request; further in,
/// the header and the bytes are fetched in parallel (one round trip either
/// way).
const HEADER_JOIN: u64 = 1 << 20;
/// The in-stream seek table is looked for in this many trailing bytes.
const SEEK_TABLE_TAIL: u64 = 64 << 10;
/// An in-stream seek table larger than this is refused (12 bytes per
/// frame: over five million frames).
const MAX_SEEK_TABLE: u64 = 64 << 20;

/// Random access into a Nexus repacked zip over HTTP range requests.
/// Opening reads the tail (which also tells the size) and, if it is not in
/// the tail, the central directory; after that every read fetches only the
/// zstd frames it needs.
#[derive(Debug)]
pub struct NexusArchive {
    remote: Remote,
    index: ZipIndex,
    /// Per entry, once known: data range and frame map.
    seekable: Vec<OnceLock<SeekableEntry>>,
    data: Vec<OnceLock<Range<u64>>>,
    /// Data ranges learned from local headers since this handle was built
    /// (not counting those restored by
    /// [`from_index_with_offsets`](NexusArchive::from_index_with_offsets)).
    learned: AtomicU64,
    /// Per entry: its data range was restored, not read from its header.
    restored: Vec<AtomicBool>,
}

/// The repacked zip on Nexus's file host.
#[derive(Debug)]
struct Remote {
    client: Arc<NexusClient>,
    uid: u64,
    len: u64,
    /// Raw bytes downloaded ahead of the reads that need them.
    spans: Mutex<Vec<Arc<Span>>>,
}

/// A prefetched range of the raw archive ([`NexusArchive::prefetch`]).
#[derive(Debug)]
struct Span {
    range: Range<u64>,
    state: watch::Receiver<SpanState>,
}

#[derive(Debug, Clone)]
enum SpanState {
    Pending,
    Ready(Arc<Vec<u8>>),
    /// The download failed: reads inside the span make their own requests
    /// (and report their own errors).
    Failed,
}

/// Keeps a prefetched span in memory; dropping it frees the bytes (and
/// cancels the download if it is still running).
#[derive(Debug)]
pub struct SpanLease {
    archive: Arc<NexusArchive>,
    span: Arc<Span>,
    task: AbortHandle,
}

impl Drop for SpanLease {
    fn drop(&mut self) {
        self.task.abort();
        lock(&self.archive.remote.spans).retain(|s| !Arc::ptr_eq(s, &self.span));
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Remote {
    fn new(client: Arc<NexusClient>, uid: u64, len: u64) -> Remote {
        Remote {
            client,
            uid,
            len,
            spans: Mutex::new(Vec::new()),
        }
    }

    /// Bytes `range` of the zip: from prefetched spans when they cover it
    /// (waiting for a span still downloading), else fetched.
    async fn get(&self, range: Range<u64>, job: &Job) -> Result<Vec<u8>> {
        if let Some(bytes) = self.buffered(range.clone()).await {
            return Ok(bytes);
        }
        self.fetch(range, job).await
    }

    /// `range` assembled from prefetched spans, if they cover all of it
    /// and downloaded fine.
    async fn buffered(&self, range: Range<u64>) -> Option<Vec<u8>> {
        if range.is_empty() {
            return None;
        }
        let chain = {
            let spans = lock(&self.spans);
            if spans.is_empty() {
                return None;
            }
            // (span, the part of `range` it supplies); spans may overlap.
            let mut chain = Vec::new();
            let mut pos = range.start;
            while pos < range.end {
                let s = spans
                    .iter()
                    .find(|s| s.range.start <= pos && pos < s.range.end)?;
                let to = s.range.end.min(range.end);
                chain.push((s.clone(), pos..to));
                pos = to;
            }
            chain
        };
        let mut out = Vec::with_capacity((range.end - range.start) as usize);
        for (s, part) in chain {
            let mut rx = s.state.clone();
            let state = rx
                .wait_for(|st| !matches!(st, SpanState::Pending))
                .await
                .ok()?
                .clone();
            let SpanState::Ready(bytes) = state else {
                return None;
            };
            let (from, to) = (part.start - s.range.start, part.end - s.range.start);
            out.extend_from_slice(bytes.get(from as usize..to as usize)?);
        }
        Some(out)
    }

    /// Fetch bytes `range` of the zip (retried; the signed URL is renewed
    /// once if the file host rejects it).
    async fn fetch(&self, range: Range<u64>, job: &Job) -> Result<Vec<u8>> {
        if range.end > self.len {
            // Nexus's file host answers ranges past EOF with a 500, never a 416.
            return Err(SourceError::Unsupported(format!(
                "range {range:?} past the end of the {}-byte archive",
                self.len
            )));
        }
        let http = self.client.http();
        // Every response must report the length the archive was opened
        // with: a renewed URL (or a re-upload) serving another file fails
        // instead of being read at the old offsets.
        let len = self.len;
        with_url(&self.client, self.uid, move |url: Url| {
            let range = range.clone();
            async move {
                http.config()
                    .retry
                    .run(job, || http.get_range(&url, range.clone(), Some(len), job))
                    .await
            }
        })
        .await
    }
}

impl NexusArchive {
    pub async fn open(client: Arc<NexusClient>, uid: u64) -> Result<NexusArchive> {
        let job = client.http().start(format!("nexus index {uid}"), None);
        let r = Self::open_inner(client.clone(), uid, &job).await;
        job.complete(r)
    }

    async fn open_inner(client: Arc<NexusClient>, uid: u64, job: &Job) -> Result<NexusArchive> {
        let http = client.http();
        // The tail first, as a suffix range: its Content-Range says how
        // long the zip is, so no request is spent on learning that. (The
        // modlist's archive size is no use here: it is the size of the
        // file the author uploaded, not of Nexus's repack of it.)
        let want = TAIL_READ.max(MAX_TAIL);
        // One attempt: whatever goes wrong with it, the requests below
        // are the retry.
        let suffix = with_url(&client, uid, move |url: Url| async move {
            http.get_suffix(&url, want, job).await
        })
        .await;
        let (remote, tail) = match suffix {
            Ok((tail, len)) => (Remote::new(client, uid, len), tail),
            // The file host did not answer the suffix range as asked (it
            // answers a range past the end with a 500, and might a suffix
            // longer than the file): ask for the length, then the tail.
            Err(e) if suffix_failed(&e) => {
                tracing::debug!(
                    uid,
                    error = %e,
                    "no answer to a suffix range; asking for the archive's length"
                );
                let len = with_url(&client, uid, move |url: Url| async move {
                    http.range_len(&url).await?.ok_or_else(|| {
                        SourceError::protocol(&url, "file host ignored the Range header")
                    })
                })
                .await?;
                let remote = Remote::new(client, uid, len);
                let tail = remote.get(len - len.min(want)..len, job).await?;
                (remote, tail)
            }
            Err(e) => return Err(e),
        };
        let len = remote.len;
        let tail_start = len - tail.len() as u64;
        let cd = match locate_central_directory(&tail, tail_start)? {
            EndRecords::Found(cd) => cd,
            EndRecords::NeedZip64Record(pos) => {
                let rec = remote.get(pos..pos + ZIP64_EOCD_LEN, job).await?;
                CentralDirectory::from_zip64_record(&rec, len)?
            }
        };
        if cd.size > MAX_CENTRAL_DIRECTORY {
            return Err(SourceError::Unsupported(format!(
                "central directory of {} bytes",
                cd.size
            )));
        }
        let cd_bytes = if cd.offset >= tail_start {
            let rel = (cd.offset - tail_start) as usize;
            tail[rel..rel + cd.size as usize].to_vec()
        } else {
            remote.get(cd.offset..cd.offset + cd.size, job).await?
        };
        let index = ZipIndex::parse_central_directory(&cd_bytes, cd.entries)?;
        Ok(Self::build(remote, index))
    }

    /// Rebuild an archive from an index saved after an earlier
    /// [`open`](Self::open) (its [`len`](Self::len) and
    /// [`index`](Self::index)), without any request. Reads fail with
    /// "archive changed on the server" if the file host's copy is no longer
    /// `len` bytes long.
    pub fn from_index(
        client: Arc<NexusClient>,
        uid: u64,
        len: u64,
        index: ZipIndex,
    ) -> Result<NexusArchive> {
        Ok(Self::build(Remote::new(client, uid, len), index))
    }

    /// [`from_index`](Self::from_index), plus each entry's data offset as
    /// [`data_offsets`](Self::data_offsets) reported it then, so reads need
    /// no local header. An offset that cannot be right is ignored (that
    /// entry reads its header again), as is a list of the wrong length:
    /// one inside the local header's fixed part and name, or whose data
    /// would run into the next local header or past the archive. So are
    /// offsets of stored (method 0) entries, whose bytes carry no checksum
    /// that would catch a stale offset; Nexus repacks store only
    /// directories that way. A restored offset is not checked against the
    /// header (that would cost the request it saves): a read that then
    /// fails to decode says so through
    /// [`offset_restored`](Self::offset_restored), and the caller should
    /// reopen the archive without offsets.
    pub fn from_index_with_offsets(
        client: Arc<NexusClient>,
        uid: u64,
        len: u64,
        index: ZipIndex,
        offsets: &[Option<u64>],
    ) -> Result<NexusArchive> {
        let a = Self::build(Remote::new(client, uid, len), index);
        if offsets.len() != a.data.len() {
            return Ok(a);
        }
        let mut starts: Vec<u64> = a
            .index
            .entries()
            .iter()
            .map(|e| e.local_header_offset)
            .collect();
        starts.sort_unstable();
        for (id, (o, e)) in offsets.iter().zip(a.index.entries()).enumerate() {
            let Some(start) = *o else { continue };
            if e.method != METHOD_ZSTD {
                continue;
            }
            let lh = e.local_header_offset;
            let next = starts[starts.partition_point(|&s| s <= lh)..]
                .first()
                .copied()
                .unwrap_or(len)
                .min(len);
            let plausible = lh
                .checked_add(LOCAL_HEADER_LEN + e.name.len() as u64)
                .is_some_and(|min| start >= min)
                && start
                    .checked_add(e.compressed_size)
                    .is_some_and(|end| end <= next);
            if plausible {
                let _ = a.data[id].set(start..start + e.compressed_size);
                a.restored[id].store(true, Ordering::Relaxed);
            }
        }
        Ok(a)
    }

    /// Whether entry `id`'s data offset was restored by
    /// [`from_index_with_offsets`](Self::from_index_with_offsets) rather
    /// than read from its local header. If a read of such an entry fails as
    /// corrupt, the offset may be stale (a re-upload of the same length).
    pub fn offset_restored(&self, id: usize) -> bool {
        self.restored
            .get(id)
            .is_some_and(|r| r.load(Ordering::Relaxed))
    }

    fn build(remote: Remote, index: ZipIndex) -> NexusArchive {
        let n = index.entries().len();
        NexusArchive {
            remote,
            index,
            seekable: (0..n).map(|_| OnceLock::new()).collect(),
            data: (0..n).map(|_| OnceLock::new()).collect(),
            restored: (0..n).map(|_| AtomicBool::new(false)).collect(),
            learned: AtomicU64::new(0),
        }
    }

    /// Per entry, the absolute offset of its compressed data once known
    /// (from a read of its local header, or restored). Save it with the
    /// index and pass it to
    /// [`from_index_with_offsets`](Self::from_index_with_offsets) next time.
    pub fn data_offsets(&self) -> Vec<Option<u64>> {
        self.data.iter().map(|d| d.get().map(|r| r.start)).collect()
    }

    /// How many data offsets this handle learned from local headers: when
    /// it grows, [`data_offsets`](Self::data_offsets) is worth saving.
    pub fn learned(&self) -> u64 {
        self.learned.load(Ordering::Relaxed)
    }

    /// Record that entry `id`'s compressed data starts at `start`, as a
    /// bulk read of its local header found (so later reads, and the saved
    /// index, need no header of their own). Ignored for an unknown entry, a
    /// start that would put the data past the archive's end, or an entry
    /// whose start is already known.
    pub fn learn_data_offset(&self, id: usize, start: u64) {
        let Some(e) = self.index.entries().get(id) else {
            return;
        };
        let lh_min = e
            .local_header_offset
            .checked_add(LOCAL_HEADER_LEN + e.name.len() as u64);
        match (lh_min, start.checked_add(e.compressed_size)) {
            (Some(min), Some(end)) if start >= min && end <= self.remote.len => {
                self.set_data(id, start..end)
            }
            _ => {}
        }
    }

    fn set_data(&self, id: usize, data: Range<u64>) {
        if self.data[id].set(data).is_ok() {
            self.learned.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn uid(&self) -> u64 {
        self.remote.uid
    }

    /// Size of the repacked zip in bytes.
    pub fn len(&self) -> u64 {
        self.remote.len
    }

    pub fn is_empty(&self) -> bool {
        self.remote.len == 0
    }

    pub fn index(&self) -> &ZipIndex {
        &self.index
    }

    /// Entry id for a Wabbajack path (`\` or `/`, any case).
    pub fn find(&self, path: &str) -> Option<usize> {
        self.index.position(path)
    }

    fn entry(&self, id: usize) -> Result<&ZipEntry> {
        self.index.entries().get(id).ok_or_else(|| {
            SourceError::Unsupported(format!(
                "no entry {id} in Nexus archive {}",
                self.remote.uid
            ))
        })
    }

    /// The raw bytes of the zip a read of decompressed bytes `range` of
    /// entry `id` would request (the local header too while the entry's
    /// layout is unknown), for [`prefetch`](Self::prefetch). `None` for an
    /// empty or out-of-range read, a directory, or a method reads refuse.
    pub fn raw_extent(&self, id: usize, range: Range<u64>) -> Result<Option<Range<u64>>> {
        let e = self.entry(id)?;
        if range.is_empty() || range.end > e.uncompressed_size {
            return Ok(None);
        }
        let rel = match e.method {
            METHOD_STORED if e.compressed_size == e.uncompressed_size => range,
            METHOD_ZSTD => {
                let table = match self.seekable[id].get() {
                    Some(s) => Some(s.table.clone()),
                    None => e.frame_map()?,
                };
                match table {
                    Some(t) => {
                        let f = t.frames_for_range(range)?;
                        let (Some(first), Some(last)) = (f.first(), f.last()) else {
                            return Ok(None);
                        };
                        first.compressed_offset..last.compressed_range().end
                    }
                    // The seek table is at the data's end, so the read
                    // fetches the tail too: prefetch the whole entry only
                    // for a whole-entry read, never for a part of it (a
                    // BSA's head would pull in all of a big entry).
                    None if range == (0..e.uncompressed_size) => 0..e.compressed_size,
                    None => return Ok(None),
                }
            }
            _ => return Ok(None),
        };
        if let Some(d) = self.data[id].get() {
            let end = d.start.checked_add(rel.end).filter(|&x| x <= d.end);
            return Ok(end.map(|end| d.start + rel.start..end));
        }
        // As `fetch_with_header` will ask for it.
        let lh = self.local_header_start(e)?;
        let min_data = lh
            .checked_add(LOCAL_HEADER_LEN)
            .and_then(|v| v.checked_add(e.name.len() as u64));
        let Some(min_data) = min_data else {
            return Ok(None);
        };
        let Some(end) = min_data
            .checked_add(LOCAL_EXTRA_GUESS)
            .and_then(|v| v.checked_add(rel.end))
        else {
            return Ok(None);
        };
        let start = if rel.start <= HEADER_JOIN {
            lh
        } else {
            min_data + rel.start
        };
        Ok(Some(start.min(self.remote.len)..end.min(self.remote.len)))
    }

    /// Download bytes `raw` of the zip in one request, in the background,
    /// and serve reads inside it from memory until the lease is dropped. A
    /// read that comes while the download runs waits for it; if it fails,
    /// reads make their own requests. Call within a tokio runtime.
    pub fn prefetch(self: &Arc<Self>, raw: Range<u64>) -> Result<SpanLease> {
        if raw.start > raw.end || raw.end > self.remote.len {
            return Err(SourceError::Unsupported(format!(
                "prefetch {raw:?} outside the {}-byte archive",
                self.remote.len
            )));
        }
        let (tx, rx) = watch::channel(SpanState::Pending);
        let span = Arc::new(Span {
            range: raw.clone(),
            state: rx,
        });
        lock(&self.remote.spans).push(span.clone());
        let a = self.clone();
        let task = tokio::spawn(async move {
            let job = a.remote.client.http().start(
                format!("nexus {} prefetch {raw:?}", a.remote.uid),
                Some(raw.end - raw.start),
            );
            let r = a.remote.fetch(raw, &job).await;
            // A failure is reported on the job's events.
            let state = match job.complete(r) {
                Ok(bytes) => SpanState::Ready(Arc::new(bytes)),
                Err(_) => SpanState::Failed,
            };
            let _ = tx.send(state);
        });
        Ok(SpanLease {
            archive: self.clone(),
            span,
            task: task.abort_handle(),
        })
    }

    /// Decompressed bytes `range` of entry `id`, fetching only the frames
    /// that cover it (adjacent frames in one request) and checking each
    /// frame's checksum.
    pub async fn read_range(&self, id: usize, range: Range<u64>) -> Result<Vec<u8>> {
        let e = self.entry(id)?;
        let job = self.remote.client.http().start(
            format!("nexus {} {} {range:?}", self.remote.uid, e.name),
            None,
        );
        let r = self.read_range_inner(id, e, range, &job).await;
        job.complete(r)
    }

    async fn read_range_inner(
        &self,
        id: usize,
        e: &ZipEntry,
        range: Range<u64>,
        job: &Job,
    ) -> Result<Vec<u8>> {
        if range.start > range.end || range.end > e.uncompressed_size {
            return Err(SourceError::Unsupported(format!(
                "{}: range {range:?} outside {} bytes",
                e.name, e.uncompressed_size
            )));
        }
        if range.is_empty() {
            return Ok(Vec::new());
        }
        match e.method {
            METHOD_STORED => {
                check_stored(e)?;
                if let Some(data) = self.data[id].get() {
                    return self
                        .remote
                        .get(data.start + range.start..data.start + range.end, job)
                        .await;
                }
                Ok(self.fetch_with_header(id, e, range, job).await?.1)
            }
            METHOD_ZSTD => {
                let (s, mut first) = match self.seekable[id].get() {
                    Some(s) => (s.clone(), None),
                    None => match (self.data[id].get(), e.frame_map()?) {
                        // The layout is unknown but the directory has the
                        // frame map: fetch the local header with the
                        // first run of frames.
                        (None, Some(t))
                            if let Some(run) =
                                runs(&t.frames_for_range(range.clone())?).into_iter().next() =>
                        {
                            let rel =
                                run[0].compressed_offset..run[run.len() - 1].compressed_range().end;
                            let (data, bytes) =
                                self.fetch_with_header(id, e, rel.clone(), job).await?;
                            let s = SeekableEntry::from_parts(e.clone(), data, t)?;
                            let _ = self.seekable[id].set(s.clone());
                            let abs = s.data.start + rel.start..s.data.start + rel.end;
                            (s, Some((abs, bytes)))
                        }
                        _ => (self.seekable(id, e, job).await?, None),
                    },
                };
                let frames = s.frames_for_range(range.clone())?;
                // Owned (run, absolute byte range, bytes already fetched)
                // triples: the per-run futures borrow nothing but `self`
                // and `job`, which keeps this future `Send`.
                let runs: Vec<Run> = runs(&frames)
                    .into_iter()
                    .map(|run| {
                        let abs = s.absolute(&run[0]).start..s.absolute(&run[run.len() - 1]).end;
                        let have = first.take_if(|(r, _)| *r == abs).map(|(_, b)| b);
                        (run, abs, have)
                    })
                    .collect();
                let mut parts = stream::iter(runs)
                    .map(|(run, abs, have)| {
                        let range = range.clone();
                        async move {
                            let bytes = match have {
                                Some(b) => b,
                                None => self.remote.get(abs, job).await?,
                            };
                            let base = run[0].compressed_offset;
                            // Frame checksum failures (a mismatch found only
                            // after decoding) are not retried: they are rare
                            // in practice and a caller that wants to retry
                            // one can call read_range again.
                            tokio::task::spawn_blocking(move || decode(&run, &bytes, base, range))
                                .await
                                .map_err(std::io::Error::other)?
                        }
                    })
                    .buffered(self.remote.client.http().config().parallel_parts.max(1));
                // `range` comes from the caller and is checked above against
                // `e.uncompressed_size`, but that size is itself an
                // unverified claim from the central directory: cap the
                // up-front allocation and let genuinely large, verified
                // reads grow it.
                let mut out =
                    Vec::with_capacity((range.end - range.start).min(MAX_REQUEST) as usize);
                while let Some(part) = parts.next().await {
                    out.extend_from_slice(&part?);
                }
                Ok(out)
            }
            m => Err(SourceError::Unsupported(format!(
                "{}: compression method {m}",
                e.name
            ))),
        }
    }

    /// The whole decompressed entry. Fetches local header and data in one
    /// request when the entry's layout is not yet known.
    pub async fn read_entry(&self, id: usize) -> Result<Vec<u8>> {
        let e = self.entry(id)?;
        let job = self.remote.client.http().start(
            format!("nexus {} {}", self.remote.uid, e.name),
            Some(e.compressed_size),
        );
        let r = async {
            if e.uncompressed_size == 0 {
                return Ok(Vec::new());
            }
            check_stored(e)?;
            if self.data[id].get().is_some() {
                return self
                    .read_range_inner(id, e, 0..e.uncompressed_size, &job)
                    .await;
            }
            let (data, compressed) = self
                .fetch_with_header(id, e, 0..e.compressed_size, &job)
                .await?;
            let (method, size, name) = (e.method, e.uncompressed_size, e.name.clone());
            // Build and cache the validated frame map so a later read_range
            // on this entry reuses it instead of re-fetching the table.
            let seekable = match method {
                METHOD_ZSTD => {
                    let t = match e.frame_map()? {
                        Some(t) => t,
                        None => SeekTable::read_from(&compressed[..])?,
                    };
                    let s = SeekableEntry::from_parts(e.clone(), data, t)?;
                    let _ = self.seekable[id].set(s.clone());
                    Some(s)
                }
                _ => None,
            };
            let out = tokio::task::spawn_blocking(move || match (method, seekable) {
                (METHOD_STORED, _) => Ok(compressed),
                // Frame checksum failures are not retried here either; see
                // the comment in read_range_inner.
                (METHOD_ZSTD, Some(s)) => decode(s.table.frames(), &compressed, 0, 0..size),
                (m, _) => Err(SourceError::Unsupported(format!(
                    "{name}: compression method {m}"
                ))),
            })
            .await
            .map_err(std::io::Error::other)??;
            if out.len() as u64 != size {
                return Err(corrupt(format!(
                    "{}: decoded {} bytes, directory says {size}",
                    e.name,
                    out.len()
                )));
            }
            Ok(out)
        }
        .await;
        job.complete(r)
    }

    /// Learn entry `id`'s data range from its local header and fetch bytes
    /// `rel` of its compressed data, in one round trip: one request from
    /// the header through `rel` when `rel` starts near the data's start,
    /// else the header and a guess at `rel`'s position in parallel. The
    /// guess allows [`LOCAL_EXTRA_GUESS`] bytes of local extra field; a
    /// header with more costs one more request for whatever was missed.
    /// Returns the data range (also remembered) and exactly `rel`'s bytes.
    async fn fetch_with_header(
        &self,
        id: usize,
        e: &ZipEntry,
        rel: Range<u64>,
        job: &Job,
    ) -> Result<(Range<u64>, Vec<u8>)> {
        let overflow = || corrupt(format!("{}: local header/entry size overflows", e.name));
        let lh = self.local_header_start(e)?;
        let len = self.remote.len;
        let min_data = lh
            .checked_add(LOCAL_HEADER_LEN)
            .and_then(|v| v.checked_add(e.name.len() as u64))
            .ok_or_else(overflow)?;
        let max_data = min_data
            .checked_add(LOCAL_EXTRA_GUESS)
            .ok_or_else(overflow)?;
        let guess_end = max_data.checked_add(rel.end).ok_or_else(overflow)?.min(len);
        let (data, mut buf, mut buf_start) = if rel.start <= HEADER_JOIN {
            let buf = self.remote.get(lh..guess_end, job).await?;
            let data = self.check_data(e, local_data_range(&buf, e)?)?;
            (data, buf, lh)
        } else {
            let body_start = min_data
                .checked_add(rel.start)
                .ok_or_else(overflow)?
                .min(guess_end);
            let (header, body) = futures_util::future::try_join(
                self.remote.get(lh..max_data.min(len), job),
                self.remote.get(body_start..guess_end, job),
            )
            .await?;
            let data = self.check_data(e, local_data_range(&header, e)?)?;
            (data, body, body_start)
        };
        if rel.end > data.end - data.start {
            return Err(corrupt(format!(
                "{}: bytes {rel:?} outside {} bytes of data",
                e.name,
                data.end - data.start
            )));
        }
        let want = data.start + rel.start..data.start + rel.end;
        let buf_end = buf_start + buf.len() as u64;
        if want.start < buf_start || want.start > buf_end {
            // The guess missed altogether (a local header unlike the
            // directory's): fetch exactly what is wanted.
            buf = self.remote.get(want.clone(), job).await?;
            buf_start = want.start;
        } else if want.end > buf_end {
            buf.extend(self.remote.get(buf_end..want.end, job).await?);
        }
        buf.truncate((want.end - buf_start) as usize);
        buf.drain(..(want.start - buf_start) as usize);
        self.set_data(id, data.clone());
        Ok((data, buf))
    }

    fn check_data(&self, e: &ZipEntry, data: Range<u64>) -> Result<Range<u64>> {
        if data.end > self.remote.len {
            return Err(corrupt(format!(
                "{}: data {data:?} runs past the {}-byte archive",
                e.name, self.remote.len
            )));
        }
        Ok(data)
    }

    /// Absolute offset of an entry's local header, checked to lie inside
    /// the archive (a corrupt central directory can claim any offset, and
    /// an offset near the archive's length would otherwise overflow the
    /// sums built from it).
    fn local_header_start(&self, e: &ZipEntry) -> Result<u64> {
        if e.local_header_offset >= self.remote.len {
            return Err(corrupt(format!(
                "{}: local header offset {} outside the {}-byte archive",
                e.name, e.local_header_offset, self.remote.len
            )));
        }
        Ok(e.local_header_offset)
    }

    /// The frame map and data range of a zstd entry. With the data range
    /// unknown and no frame map in the directory, the local header and the
    /// data's tail (where the seek table is) come in one round trip.
    async fn seekable(&self, id: usize, e: &ZipEntry, job: &Job) -> Result<SeekableEntry> {
        if let Some(s) = self.seekable[id].get() {
            return Ok(s.clone());
        }
        let csize = e.compressed_size;
        let tail_rel = csize - csize.min(SEEK_TABLE_TAIL)..csize;
        let (data, table) = match (e.frame_map()?, self.data[id].get()) {
            (Some(t), Some(data)) => (data.clone(), t),
            (Some(t), None) => (self.fetch_with_header(id, e, 0..0, job).await?.0, t),
            (None, known) => {
                // No 0x4E58 copy: read the seek table from the end of the data.
                let (data, tail) = match known {
                    Some(data) => (
                        data.clone(),
                        self.remote
                            .get(data.start + tail_rel.start..data.end, job)
                            .await?,
                    ),
                    None => self.fetch_with_header(id, e, tail_rel, job).await?,
                };
                let table = match SeekTable::parse(&tail) {
                    Ok(t) => t,
                    // The first parse can fail either because the fetched
                    // tail is too short to hold the whole table (genuinely
                    // recoverable: fetch more, from further back) or
                    // because the entry's data is corrupt or too small to
                    // hold even the 9-byte footer. Only the first case can
                    // be retried; the second must not slice past what we
                    // fetched.
                    Err(_) if (tail.len() as u64) < FOOTER_LEN as u64 => {
                        return Err(corrupt(format!(
                            "{}: {} bytes of entry data is shorter than the seek table footer",
                            e.name,
                            tail.len()
                        )));
                    }
                    Err(_) => {
                        let need = SeekTable::table_frame_len(&tail[tail.len() - FOOTER_LEN..])?;
                        if need > MAX_SEEK_TABLE {
                            return Err(corrupt(format!(
                                "{}: seek table of {need} bytes (limit {MAX_SEEK_TABLE})",
                                e.name
                            )));
                        }
                        let from = data
                            .end
                            .checked_sub(need)
                            .filter(|&f| f >= data.start)
                            .ok_or_else(|| {
                                corrupt(format!("{}: seek table longer than the entry", e.name))
                            })?;
                        SeekTable::parse(&self.remote.get(from..data.end, job).await?)?
                    }
                };
                (data, table)
            }
        };
        let s = SeekableEntry::from_parts(e.clone(), data, table)?;
        let _ = self.seekable[id].set(s.clone());
        Ok(s)
    }
}

/// A run of adjacent frames, its absolute byte range, and its bytes if
/// they were already fetched.
type Run = (Vec<FrameEntry>, Range<u64>, Option<Vec<u8>>);

/// A corrupt or self-inconsistent zip (a central directory or seek table
/// that does not describe real data): never worth retrying, and distinct
/// from [`SourceError::Unsupported`], which means we understood the data
/// but chose not to handle it.
fn corrupt(msg: impl Into<String>) -> SourceError {
    SourceError::Archive {
        format: "zip",
        msg: msg.into(),
    }
}

/// A stored (uncompressed) entry's compressed and uncompressed sizes must
/// be equal; a corrupt central directory can claim otherwise.
fn check_stored(e: &ZipEntry) -> Result<()> {
    if e.method == METHOD_STORED && e.compressed_size != e.uncompressed_size {
        return Err(corrupt(format!(
            "{}: stored entry has compressed size {} != uncompressed size {}",
            e.name, e.compressed_size, e.uncompressed_size
        )));
    }
    Ok(())
}

/// Whether a suffix-range request failed at the file host (it would not
/// serve one, answered with something else than the range asked for, or
/// the request did not get through), so that asking again another way may
/// work. Not a link the host rejects twice (403), nor an error of the API
/// that signs the link: those would only fail again.
fn suffix_failed(e: &SourceError) -> bool {
    match e {
        SourceError::Status { status, .. } => *status != 403,
        SourceError::Protocol { .. }
        | SourceError::Network { .. }
        | SourceError::CorruptPart { .. } => true,
        _ => false,
    }
}

/// Whether `e`, from the file host, says the signed link itself is no
/// good: a client error other than the ones that are about the request
/// (408 timeout, 416 range) or the file (404 and 429 are errors of their
/// own). A link refused this way must not be used again, least of all by
/// the next run, which would find it in the link file.
fn link_refused(e: &SourceError) -> bool {
    matches!(e, SourceError::Status { status, .. }
        if (400..500).contains(status) && !matches!(status, 408 | 416))
}

/// Run `op` with the current signed URL; if the file host refuses the link
/// ([`link_refused`]: a 403, usually), forget that link, get a fresh one
/// and run `op` once more.
pub(crate) async fn with_url<T, F, Fut>(client: &NexusClient, uid: u64, op: F) -> Result<T>
where
    F: Fn(Url) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let url = client.download_url(uid).await?;
    match op(url.clone()).await {
        Err(e) if link_refused(&e) => {
            // Only this link: another reader may have renewed it already.
            client.forget_refused(uid, &url);
            op(client.download_url(uid).await?).await
        }
        other => other,
    }
}

/// Split `frames` (contiguous, in order) into runs of at most
/// `MAX_REQUEST` compressed bytes, each fetched with one request.
fn runs(frames: &[FrameEntry]) -> Vec<Vec<FrameEntry>> {
    let mut out: Vec<Vec<FrameEntry>> = Vec::new();
    for f in frames {
        match out.last_mut() {
            Some(run)
                if run[run.len() - 1].compressed_range().end == f.compressed_offset
                    && f.compressed_range().end - run[0].compressed_offset <= MAX_REQUEST =>
            {
                run.push(f.clone())
            }
            _ => out.push(vec![f.clone()]),
        }
    }
    out
}

/// Decompress `frames` from `bytes` (which start at compressed offset
/// `base`) and return the part inside `range`.
fn decode(frames: &[FrameEntry], bytes: &[u8], base: u64, range: Range<u64>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut scratch = Vec::new();
    for f in frames {
        if f.decompressed_size == 0 {
            continue;
        }
        let c = f.compressed_range();
        let (a, b) = ((c.start - base) as usize, (c.end - base) as usize);
        let compressed = bytes
            .get(a..b)
            .ok_or_else(|| corrupt(format!("frame {} outside the fetched bytes", f.index)))?;
        scratch.resize(f.decompressed_size as usize, 0);
        decompress_frame_into(f, compressed, &mut scratch)?;
        let d = f.decompressed_range();
        let from = range.start.max(d.start) - d.start;
        let to = range.end.min(d.end) - d.start;
        if from < to {
            out.extend_from_slice(&scratch[from as usize..to as usize]);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(i: usize, off: u64, size: u32) -> FrameEntry {
        FrameEntry {
            index: i,
            compressed_offset: off,
            compressed_size: size,
            decompressed_offset: i as u64 * 100,
            decompressed_size: 100,
            checksum: None,
        }
    }

    #[test]
    fn runs_coalesce_adjacent_frames_up_to_the_cap() {
        let f = [frame(0, 0, 10), frame(1, 10, 10), frame(2, 20, 10)];
        assert_eq!(runs(&f).len(), 1);
        let big = MAX_REQUEST as u32 / 2 + 1;
        let g = [
            frame(0, 0, big),
            frame(1, big as u64, big),
            frame(2, 2 * big as u64, 5),
        ];
        let r = runs(&g);
        assert_eq!(r.iter().map(Vec::len).collect::<Vec<_>>(), [1, 2]);
    }
}
