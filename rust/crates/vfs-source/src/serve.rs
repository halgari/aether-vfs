//! Serve any in-process [`Provider`] as a gRPC SourceService.

use std::sync::Arc;

use tonic::{Request, Response, Status};
use vfs_provider::{
    Access, Provider, RootId, VPath, KIND_DIR, OPEN_CREATE, OPEN_EXCL, OPEN_TRUNC, OPEN_WRITE,
    ST_BAD_REQUEST, ST_OK, ST_READ_ONLY,
};

use crate::pb::source_server::Source;
use crate::pb::{
    CapsResp, DirEnt, Empty, GetAttrReq, GetAttrResp, OpenReq, OpenResp, ReadDirReq, ReadDirResp,
    ReadReq, ReadResp, ReleaseReq,
};

pub struct ProviderSourceService {
    provider: Arc<dyn Provider>,
}

impl ProviderSourceService {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider }
    }
}

/// Reject a wire path before it ever reaches a provider.
///
/// `VPath::rel` is documented (`vfs-provider/src/path.rs`) as normalized,
/// forward-slash, no leading slash — but nothing on the gRPC boundary
/// enforced that until now. A client is free to send anything in the `path`
/// field, and `vfs-source-plugin` binds a TCP port, so a `..`-escaping path
/// must be refused here rather than trusted to reach a well-behaved provider.
/// `DiskProvider::resolve` now also rejects a bare `..` component itself
/// (defense in depth for anything reaching it directly, in-process), but the
/// gRPC boundary is the first line of defense for a network client.
///
/// Rejects (returns `false` for): a `..` path component (matched
/// component-wise, so a file legitimately named `..foo` is unaffected), a
/// leading `/`, any backslash, and any `:`. Everything else is accepted
/// as-is — this validates, it does not sanitize.
fn is_wire_path_safe(path: &str) -> bool {
    if path.starts_with('/') || path.contains('\\') || path.contains(':') {
        return false;
    }
    !path.split('/').any(|segment| segment == "..")
}

#[tonic::async_trait]
impl Source for ProviderSourceService {
    async fn get_capabilities(&self, _req: Request<Empty>) -> Result<Response<CapsResp>, Status> {
        // The Stage-1 wire contract has no write RPCs (no Write/SetLen/Flush/
        // Mkdir/Remove/Rename/SetAttr messages) — so advertising ReadWrite
        // here would promise a capability the transport cannot keep. Clamp
        // to what can actually cross the wire; the write half arrives with
        // Stage 3's proto extension.
        let caps = self.provider.capabilities().read_only_clamp();
        let access = match caps.access {
            Access::SeqRead => 0,
            Access::Read => 1,
            Access::ReadWrite => 2,
        };
        Ok(Response::new(CapsResp {
            contract_version: crate::SOURCE_CONTRACT_VERSION,
            access,
            immutable: caps.immutable,
            slow: caps.slow,
            preferred_block: caps.preferred_block.unwrap_or(0),
        }))
    }

    async fn get_attr(&self, req: Request<GetAttrReq>) -> Result<Response<GetAttrResp>, Status> {
        let r = req.into_inner();
        if !is_wire_path_safe(&r.path) {
            return Ok(Response::new(GetAttrResp {
                found: false,
                is_dir: false,
                size: 0,
                mtime: 0,
                status: ST_BAD_REQUEST,
            }));
        }
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
        if !is_wire_path_safe(&r.path) {
            return Ok(Response::new(ReadDirResp {
                entries: vec![],
                status: ST_BAD_REQUEST,
            }));
        }
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
        if !is_wire_path_safe(&r.path) {
            return Ok(Response::new(OpenResp {
                handle: 0,
                size: 0,
                is_dir: false,
                file_id: 0,
                status: ST_BAD_REQUEST,
            }));
        }
        // The Stage-1 wire contract carries no write RPCs (no Write/SetLen/
        // Flush/Mkdir/Remove/Rename/SetAttr messages), so a write open can
        // never be honoured end-to-end even when the wrapped provider is
        // itself ReadWrite (get_capabilities already clamps what is
        // advertised, but that's just a promise — this is the enforcement).
        // Refuse before the provider ever sees the request, rather than
        // creating/truncating a file server-side and only then reporting a
        // capability that doesn't match what just happened.
        if r.flags & (OPEN_WRITE | OPEN_CREATE | OPEN_EXCL | OPEN_TRUNC) != 0 {
            return Ok(Response::new(OpenResp {
                handle: 0,
                size: 0,
                is_dir: false,
                file_id: 0,
                status: ST_READ_ONLY,
            }));
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_provider::conformance::MemFixture;

    fn svc() -> ProviderSourceService {
        ProviderSourceService::new(Arc::new(MemFixture::new()))
    }

    #[tokio::test]
    async fn rejects_a_dotdot_traversal() {
        let resp = svc()
            .get_attr(Request::new(GetAttrReq {
                path: "../../../Windows/System32/x".into(),
                root: 0,
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.status, ST_BAD_REQUEST);
    }

    #[tokio::test]
    async fn rejects_a_bare_dotdot_component() {
        let resp = svc()
            .get_attr(Request::new(GetAttrReq { path: "..".into(), root: 0 }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.status, ST_BAD_REQUEST);
    }

    #[tokio::test]
    async fn rejects_a_leading_slash() {
        let resp = svc()
            .read_dir(Request::new(ReadDirReq { path: "/etc".into(), root: 0 }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.status, ST_BAD_REQUEST);
    }

    #[tokio::test]
    async fn rejects_a_backslash() {
        let resp = svc()
            .open(Request::new(OpenReq {
                path: "sub\\b.txt".into(),
                flags: vfs_provider::OPEN_READ,
                root: 0,
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.status, ST_BAD_REQUEST);
    }

    #[tokio::test]
    async fn rejects_a_colon() {
        let resp = svc()
            .get_attr(Request::new(GetAttrReq { path: "C:/Windows".into(), root: 0 }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.status, ST_BAD_REQUEST);
    }

    #[tokio::test]
    async fn accepts_a_legitimate_nested_path() {
        let resp = svc()
            .get_attr(Request::new(GetAttrReq { path: "sub/b.txt".into(), root: 0 }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.status, ST_OK);
        assert!(resp.found, "sub/b.txt should have reached the provider");
        assert_eq!(resp.size, 6);
    }

    #[tokio::test]
    async fn accepts_a_dotdot_prefixed_filename() {
        // ".." as a whole path component is rejected; ".." as the start of a
        // longer segment name (a legitimate filename) must reach the
        // provider unmolested.
        let resp = svc()
            .get_attr(Request::new(GetAttrReq { path: "..foo".into(), root: 0 }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            resp.status, ST_OK,
            "a filename starting with .. must not be rejected at the boundary"
        );
        assert!(
            !resp.found,
            "..foo isn't actually in the fixture, but it must reach the provider to find that out"
        );
    }

    #[tokio::test]
    async fn a_write_open_is_refused_even_against_a_writable_provider() {
        // The wrapped provider really is ReadWrite (a real DiskProvider, not
        // the read-only MemFixture the other tests use) — this proves the
        // refusal is enforced at the RPC boundary, not just a side effect of
        // the backing provider being read-only.
        let dir = std::env::temp_dir().join(format!("vfs-source-write-refuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let svc = ProviderSourceService::new(Arc::new(vfs_director::DiskProvider::new(&dir)));

        let resp = svc
            .open(Request::new(OpenReq {
                path: "new_file.txt".into(),
                flags: vfs_provider::OPEN_WRITE | vfs_provider::OPEN_CREATE,
                root: 0,
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(
            resp.status, ST_READ_ONLY,
            "a write open must be refused with ST_READ_ONLY, not dispatched"
        );
        assert!(
            !dir.join("new_file.txt").exists(),
            "the refusal must happen before the file is created on disk, not after"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
