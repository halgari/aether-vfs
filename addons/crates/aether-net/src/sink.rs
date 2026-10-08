use std::io;
use std::sync::{Arc, Mutex};

use crate::error::Result;

/// Where a whole-file download lands. The store implements this over a
/// spool layer file; [`MemorySink`] is for tests and small files.
///
/// Calls may block (disk I/O, fsync). Sources always call them from
/// `tokio::task::spawn_blocking`, so implementations never run on a tokio
/// worker thread. Writes arrive in increasing offset order and never overlap,
/// but a retried download starts again at offset 0.
pub trait BlobSink: Send + Sync + 'static {
    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()>;
}

/// An in-memory sink that grows to fit.
#[derive(Debug, Default)]
pub struct MemorySink(Mutex<Vec<u8>>);

impl MemorySink {
    pub fn new() -> Arc<MemorySink> {
        Arc::new(MemorySink::default())
    }
    pub fn contents(&self) -> Vec<u8> {
        self.0.lock().expect("sink mutex poisoned").clone()
    }
}

impl BlobSink for MemorySink {
    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut v = self.0.lock().expect("sink mutex poisoned");
        let end = offset as usize + data.len();
        if v.len() < end {
            v.resize(end, 0);
        }
        v[offset as usize..end].copy_from_slice(data);
        Ok(())
    }
}

/// Write `data` at `offset` on a blocking thread.
#[doc(hidden)]
pub async fn write_blocking(sink: &Arc<dyn BlobSink>, offset: u64, data: Vec<u8>) -> Result<()> {
    let sink = sink.clone();
    tokio::task::spawn_blocking(move || sink.write_at(offset, &data))
        .await
        .map_err(io::Error::other)??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_sink_grows_and_overwrites() {
        let s = MemorySink::new();
        s.write_at(4, b"cd").unwrap();
        s.write_at(0, b"ab").unwrap();
        assert_eq!(s.contents(), b"ab\0\0cd");
        s.write_at(0, b"XY").unwrap();
        assert_eq!(s.contents(), b"XY\0\0cd");
    }

    #[tokio::test]
    async fn write_blocking_reaches_the_sink() {
        let s = MemorySink::new();
        let dynsink: Arc<dyn BlobSink> = s.clone();
        write_blocking(&dynsink, 1, b"z".to_vec()).await.unwrap();
        assert_eq!(s.contents(), b"\0z");
    }
}
