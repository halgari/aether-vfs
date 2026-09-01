//! gRPC [`RemoteProvider`]: any-language out-of-process provider via SourceService.

use std::sync::Mutex;

use tonic::transport::Channel;
use vfs_provider::{
    map_io_err, Access, Capabilities, CaseMatch, DirEntry, Handle, Provider, Stat, VPath,
    KIND_DIR, KIND_FILE,
};

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
        if caps_resp.contract_version != crate::SOURCE_CONTRACT_VERSION {
            return Err(format!(
                "remote source: contract version mismatch (expected v{}, got v{})",
                crate::SOURCE_CONTRACT_VERSION,
                caps_resp.contract_version
            ));
        }
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
            // `CapsResp` (source.proto) has no case field yet, so this side
            // cannot observe what the remote backend actually does with a
            // differently-cased spelling — it never sees the backend's
            // filesystem or index, only whatever the wire hands back.
            // `Sensitive` is the right answer for that: under this
            // increment's own contract, `Insensitive` is a promise the
            // *provider* makes about its own behavior, and `Sensitive` is
            // simply the absence of one — never a claim that the backend is
            // byte-exact. A `RemoteProvider` cannot make that promise on
            // another process's behalf, however likely it is to hold in
            // practice (DiskProvider on NTFS today, MemoryProvider after
            // case-fold task 3). Carrying `case` on the wire, so this is
            // actually verified instead of assumed, is the real fix,
            // deferred to a later increment.
            case: CaseMatch::Sensitive,
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tonic::{Request, Response, Status};

    use crate::pb::source_server::{Source, SourceServer};
    use crate::pb::{
        CapsResp, Empty, GetAttrReq, GetAttrResp, OpenReq, OpenResp, ReadDirReq, ReadDirResp,
        ReadReq, ReadResp, ReleaseReq,
    };

    /// Announces a contract version other than [`crate::SOURCE_CONTRACT_VERSION`].
    /// Every other RPC panics: `connect` must reject the version before
    /// issuing any of them.
    struct WrongVersionService;

    #[tonic::async_trait]
    impl Source for WrongVersionService {
        async fn get_capabilities(&self, _req: Request<Empty>) -> Result<Response<CapsResp>, Status> {
            Ok(Response::new(CapsResp {
                contract_version: crate::SOURCE_CONTRACT_VERSION + 1,
                access: 1,
                immutable: false,
                slow: false,
                preferred_block: 0,
            }))
        }
        async fn get_attr(&self, _req: Request<GetAttrReq>) -> Result<Response<GetAttrResp>, Status> {
            unreachable!("connect must reject the version before any other RPC")
        }
        async fn read_dir(&self, _req: Request<ReadDirReq>) -> Result<Response<ReadDirResp>, Status> {
            unreachable!("connect must reject the version before any other RPC")
        }
        async fn open(&self, _req: Request<OpenReq>) -> Result<Response<OpenResp>, Status> {
            unreachable!("connect must reject the version before any other RPC")
        }
        async fn read(&self, _req: Request<ReadReq>) -> Result<Response<ReadResp>, Status> {
            unreachable!("connect must reject the version before any other RPC")
        }
        async fn release(&self, _req: Request<ReleaseReq>) -> Result<Response<Empty>, Status> {
            unreachable!("connect must reject the version before any other RPC")
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connect_rejects_a_mismatched_contract_version() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(SourceServer::new(WrongVersionService))
                .serve_with_incoming(incoming)
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let err = match RemoteProvider::connect(&format!("{addr}")).await {
            Err(e) => e,
            Ok(_) => panic!("a server announcing the wrong contract version must not connect"),
        };

        let expected = crate::SOURCE_CONTRACT_VERSION;
        let received = crate::SOURCE_CONTRACT_VERSION + 1;
        assert!(
            err.contains(&format!("v{expected}")) && err.contains(&format!("v{received}")),
            "error should name both the expected and received version, got: {err}"
        );

        server.abort();
    }
}
