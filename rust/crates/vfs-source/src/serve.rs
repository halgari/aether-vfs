//! Serve any in-process [`Backend`] as a gRPC SourceService.

use std::sync::Arc;

use tonic::{Request, Response, Status};
use vfs_protocol::{Backend, KIND_DIR, ST_OK};

use crate::pb::source_server::Source;
use crate::pb::{
    DirEnt, Empty, GetAttrReq, GetAttrResp, OpenReq, OpenResp, ReadDirReq, ReadDirResp, ReadReq,
    ReadResp, ReleaseReq,
};

pub struct BackendSourceService {
    backend: Arc<dyn Backend>,
}

impl BackendSourceService {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }
}

#[tonic::async_trait]
impl Source for BackendSourceService {
    async fn get_attr(&self, req: Request<GetAttrReq>) -> Result<Response<GetAttrResp>, Status> {
        let path = req.into_inner().path;
        let be = Arc::clone(&self.backend);
        let result = tokio::task::spawn_blocking(move || be.getattr(&path))
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
        let path = req.into_inner().path;
        let be = Arc::clone(&self.backend);
        let result = tokio::task::spawn_blocking(move || be.readdir(&path))
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
        let be = Arc::clone(&self.backend);
        let result = tokio::task::spawn_blocking(move || be.open(&r.path, r.flags))
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
        let be = Arc::clone(&self.backend);
        let result = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; r.len as usize];
            match be.read(r.handle, r.offset, &mut buf) {
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
        let be = Arc::clone(&self.backend);
        let _ = tokio::task::spawn_blocking(move || be.release(h))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(Empty {}))
    }
}
