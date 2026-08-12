//! gRPC [`RemoteBackend`]: any-language out-of-process source via SourceService.

use std::sync::Mutex;

use tonic::transport::Channel;
use vfs_protocol::{map_io_err, Backend, BackendHandle, DirEntry, Stat, KIND_DIR, KIND_FILE};

use crate::pb::source_client::SourceClient;
use crate::pb::{GetAttrReq, OpenReq, ReadDirReq, ReadReq, ReleaseReq};
use crate::rt::block_on;

/// Backend that forwards every op to a remote `Source` gRPC server.
pub struct RemoteBackend {
    client: Mutex<SourceClient<Channel>>,
}

impl RemoteBackend {
    pub async fn connect(endpoint: &str) -> Result<Self, String> {
        let uri = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            endpoint.to_string()
        } else {
            format!("http://{endpoint}")
        };
        let client = SourceClient::connect(uri)
            .await
            .map_err(|e| format!("remote source connect: {e}"))?;
        Ok(Self {
            client: Mutex::new(client),
        })
    }

    pub fn connect_blocking(endpoint: &str) -> Result<Self, String> {
        block_on(Self::connect(endpoint))
    }
}

fn map_status(status: i32) -> Result<(), i32> {
    if status == 0 {
        Ok(())
    } else {
        Err(status)
    }
}

impl Backend for RemoteBackend {
    fn getattr(&self, path: &str) -> Result<Option<Stat>, i32> {
        let mut c = self.client.lock().map_err(|_| map_io_err())?;
        let resp = block_on(c.get_attr(GetAttrReq {
            path: path.to_string(),
        }))
        .map_err(|_| map_io_err())?
        .into_inner();
        map_status(resp.status)?;
        if !resp.found {
            return Ok(None);
        }
        Ok(Some(Stat {
            kind: if resp.is_dir { KIND_DIR } else { KIND_FILE },
            size: resp.size,
            mtime: resp.mtime,
        }))
    }

    fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, i32> {
        let mut c = self.client.lock().map_err(|_| map_io_err())?;
        let resp = block_on(c.read_dir(ReadDirReq {
            path: path.to_string(),
        }))
        .map_err(|_| map_io_err())?
        .into_inner();
        map_status(resp.status)?;
        Ok(resp
            .entries
            .into_iter()
            .map(|e| DirEntry {
                name: e.name,
                stat: Stat {
                    kind: if e.is_dir { KIND_DIR } else { KIND_FILE },
                    size: e.size,
                    mtime: e.mtime,
                },
            })
            .collect())
    }

    fn open(&self, path: &str, flags: u32) -> Result<(BackendHandle, u64, bool), i32> {
        let mut c = self.client.lock().map_err(|_| map_io_err())?;
        let resp = block_on(c.open(OpenReq {
            path: path.to_string(),
            flags,
        }))
        .map_err(|_| map_io_err())?
        .into_inner();
        map_status(resp.status)?;
        Ok((resp.handle, resp.size, resp.is_dir))
    }

    fn read(&self, bh: BackendHandle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let mut c = self.client.lock().map_err(|_| map_io_err())?;
        let resp = block_on(c.read(ReadReq {
            handle: bh,
            offset,
            len: buf.len() as u32,
        }))
        .map_err(|_| map_io_err())?
        .into_inner();
        map_status(resp.status)?;
        let n = resp.data.len().min(buf.len());
        buf[..n].copy_from_slice(&resp.data[..n]);
        Ok(n)
    }

    fn release(&self, bh: BackendHandle) -> Result<(), i32> {
        let mut c = self.client.lock().map_err(|_| map_io_err())?;
        let resp = block_on(c.release(ReleaseReq { handle: bh }))
            .map_err(|_| map_io_err())?
            .into_inner();
        let _ = resp;
        Ok(())
    }
}


