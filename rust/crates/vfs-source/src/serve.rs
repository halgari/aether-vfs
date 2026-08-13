//! Serve any in-process [`Provider`] as a gRPC SourceService.

use std::sync::Arc;

use tonic::{Request, Response, Status};
use vfs_provider::{Access, Provider, RootId, VPath, KIND_DIR, ST_OK};

use crate::pb::source_server::Source;
use crate::pb::{
    CapsResp, DirEnt, Empty, GetAttrReq, GetAttrResp, OpenReq, OpenResp, ReadDirReq, ReadDirResp,
    ReadReq, ReadResp, ReleaseReq,
};

/// Contract version this server speaks. Bumped when the wire shape changes.
const CONTRACT_VERSION: u32 = 1;

pub struct ProviderSourceService {
    provider: Arc<dyn Provider>,
}

impl ProviderSourceService {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider }
    }
}

#[tonic::async_trait]
impl Source for ProviderSourceService {
    async fn get_capabilities(&self, _req: Request<Empty>) -> Result<Response<CapsResp>, Status> {
        let caps = self.provider.capabilities();
        let access = match caps.access {
            Access::SeqRead => 0,
            Access::Read => 1,
            Access::ReadWrite => 2,
        };
        Ok(Response::new(CapsResp {
            contract_version: CONTRACT_VERSION,
            access,
            immutable: caps.immutable,
            slow: caps.slow,
            preferred_block: caps.preferred_block.unwrap_or(0),
        }))
    }

    async fn get_attr(&self, req: Request<GetAttrReq>) -> Result<Response<GetAttrResp>, Status> {
        let r = req.into_inner();
        let provider = Arc::clone(&self.provider);
        let result = tokio::task::spawn_blocking(move || {
            provider.getattr(VPath::new(RootId(r.root), &r.path))
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
        match result {
            Ok(Some(st)) => Ok(Response::new(GetAttrResp {
                found: true,
                is_dir: st.kind == KIND_DIR,
                size: st.size,
                mtime: st.mtime,
                status: ST_OK,
            })),
            Ok(None) => Ok(Response::new(GetAttrResp {
                found: false,
                is_dir: false,
                size: 0,
                mtime: 0,
                status: ST_OK,
            })),
            Err(status) => Ok(Response::new(GetAttrResp {
                found: false,
                is_dir: false,
                size: 0,
                mtime: 0,
                status,
            })),
        }
    }

    async fn read_dir(&self, req: Request<ReadDirReq>) -> Result<Response<ReadDirResp>, Status> {
        let r = req.into_inner();
        let provider = Arc::clone(&self.provider);
        let result = tokio::task::spawn_blocking(move || {
            provider.readdir(VPath::new(RootId(r.root), &r.path))
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
        match result {
            Ok(entries) => Ok(Response::new(ReadDirResp {
                entries: entries
                    .into_iter()
                    .map(|e| DirEnt {
                        name: e.name,
                        is_dir: e.stat.kind == KIND_DIR,
                        size: e.stat.size,
                        mtime: e.stat.mtime,
                    })
                    .collect(),
                status: ST_OK,
            })),
            Err(status) => Ok(Response::new(ReadDirResp {
                entries: vec![],
                status,
            })),
        }
    }

    async fn open(&self, req: Request<OpenReq>) -> Result<Response<OpenResp>, Status> {
        let r = req.into_inner();
        let provider = Arc::clone(&self.provider);
        let result = tokio::task::spawn_blocking(move || {
            provider.open(VPath::new(RootId(r.root), &r.path), r.flags)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
        match result {
            Ok((handle, size, is_dir)) => Ok(Response::new(OpenResp {
                handle,
                size,
                is_dir,
                file_id: 0,
                status: ST_OK,
            })),
            Err(status) => Ok(Response::new(OpenResp {
                handle: 0,
                size: 0,
                is_dir: false,
                file_id: 0,
                status,
            })),
        }
    }

    async fn read(&self, req: Request<ReadReq>) -> Result<Response<ReadResp>, Status> {
        let r = req.into_inner();
        let provider = Arc::clone(&self.provider);
        let result = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; r.len as usize];
            match provider.read_at(r.handle, r.offset, &mut buf) {
                Ok(n) => {
                    buf.truncate(n);
                    Ok(buf)
                }
                Err(s) => Err(s),
            }
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
        match result {
            Ok(data) => Ok(Response::new(ReadResp {
                data,
                status: ST_OK,
            })),
            Err(status) => Ok(Response::new(ReadResp {
                data: vec![],
                status,
            })),
        }
    }

    async fn release(&self, req: Request<ReleaseReq>) -> Result<Response<Empty>, Status> {
        let h = req.into_inner().handle;
        let provider = Arc::clone(&self.provider);
        let _ = tokio::task::spawn_blocking(move || provider.close(h))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(Empty {}))
    }
}
