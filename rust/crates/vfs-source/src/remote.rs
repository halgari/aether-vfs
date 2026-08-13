//! gRPC [`RemoteProvider`]: any-language out-of-process provider via SourceService.

use std::sync::Mutex;

use tonic::transport::Channel;
use vfs_provider::{map_io_err, Access, Capabilities, DirEntry, Handle, Provider, Stat, VPath, KIND_DIR, KIND_FILE};

use crate::pb::source_client::SourceClient;
use crate::pb::{Empty, GetAttrReq, OpenReq, ReadDirReq, ReadReq, ReleaseReq};
use crate::rt::block_on;

/// Provider that forwards every op to a remote `Source` gRPC server.
pub struct RemoteProvider {
    client: Mutex<SourceClient<Channel>>,
    /// Fetched once during `connect`/`connect_blocking`. The contract requires
    /// capabilities to be constant for the provider's lifetime, and re-fetching
    /// per call would put a network round trip on a hot path.
    caps: Capabilities,
}

impl RemoteProvider {
    pub async fn connect(endpoint: &str) -> Result<Self, String> {
        let uri = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            endpoint.to_string()
        } else {
            format!("http://{endpoint}")
        };
        let mut client = SourceClient::connect(uri)
            .await
            .map_err(|e| format!("remote source connect: {e}"))?;

        let caps_resp = client
            .get_capabilities(Empty {})
            .await
            .map_err(|e| format!("remote source get_capabilities: {e}"))?
            .into_inner();
        let access = match caps_resp.access {
            0 => Access::SeqRead,
            1 => Access::Read,
            2 => Access::ReadWrite,
            other => {
                return Err(format!(
                    "remote source: contract v{} sent unrecognized access value {other} \
                     (expected 0=SeqRead, 1=Read, 2=ReadWrite)",
                    caps_resp.contract_version
                ));
            }
        };
        let caps = Capabilities {
            access,
            immutable: caps_resp.immutable,
            slow: caps_resp.slow,
            preferred_block: (caps_resp.preferred_block != 0).then_some(caps_resp.preferred_block),
        };

        Ok(Self {
            client: Mutex::new(client),
            caps,
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

impl Provider for RemoteProvider {
    fn capabilities(&self) -> Capabilities {
        self.caps
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        let path = p.rel;
        let mut c = self.client.lock().map_err(|_| map_io_err())?;
        let resp = block_on(c.get_attr(GetAttrReq {
            path: path.to_string(),
            root: p.root.0,
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

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let path = p.rel;
        let mut c = self.client.lock().map_err(|_| map_io_err())?;
        let resp = block_on(c.read_dir(ReadDirReq {
            path: path.to_string(),
            root: p.root.0,
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

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let path = p.rel;
        let mut c = self.client.lock().map_err(|_| map_io_err())?;
        let resp = block_on(c.open(OpenReq {
            path: path.to_string(),
            flags,
            root: p.root.0,
        }))
        .map_err(|_| map_io_err())?
        .into_inner();
        map_status(resp.status)?;
        Ok((resp.handle, resp.size, resp.is_dir))
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let mut c = self.client.lock().map_err(|_| map_io_err())?;
        let resp = block_on(c.read(ReadReq {
            handle: h,
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

    fn close(&self, h: Handle) -> Result<(), i32> {
        let mut c = self.client.lock().map_err(|_| map_io_err())?;
        let resp = block_on(c.release(ReleaseReq { handle: h }))
            .map_err(|_| map_io_err())?
            .into_inner();
        let _ = resp;
        Ok(())
    }
}
