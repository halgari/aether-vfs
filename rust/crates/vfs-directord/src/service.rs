//! tonic [`Director`] service implementation.

use std::collections::BTreeMap;
use std::pin::Pin;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

use vfs_control::pb::director_server::Director;
use vfs_control::pb::{
    launch_event, source_spec, AddSourceReq, CreateSessionReq, Empty, HealthReq, HealthResp,
    LaunchEvent, LaunchReq, Session, SessionList, SourceRef, StatsResp, TeardownReq,
};
use vfs_control::SourceSpec;
use vfs_director::LaunchOpts;
use vfs_source::build_backend;

use crate::registry::SessionRegistry;

pub struct DirectorService {
    registry: SessionRegistry,
}

impl DirectorService {
    pub fn new(registry: SessionRegistry) -> Self {
        Self { registry }
    }
}

#[tonic::async_trait]
impl Director for DirectorService {
    async fn health(&self, _req: Request<HealthReq>) -> Result<Response<HealthResp>, Status> {
        Ok(Response::new(HealthResp {
            version: env!("CARGO_PKG_VERSION").to_string(),
            sessions: self.registry.len() as u32,
        }))
    }

    async fn create_session(
        &self,
        req: Request<CreateSessionReq>,
    ) -> Result<Response<Session>, Status> {
        let name = req.into_inner().name;
        let summary = self
            .registry
            .create(name)
            .map_err(|e| Status::internal(e))?;
        Ok(Response::new(Session {
            id: summary.id,
            name: summary.name,
            root: summary.root.to_string_lossy().into_owned(),
        }))
    }

    async fn add_source(&self, req: Request<AddSourceReq>) -> Result<Response<SourceRef>, Status> {
        let r = req.into_inner();
        let spec = pb_to_source_spec(r.source.as_ref())
            .map_err(|e| Status::invalid_argument(e))?;
        // build_backend may block (remote connect); run off the async executor.
        let backend = tokio::task::spawn_blocking(move || build_backend(&spec))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        let mount = if r.mount.is_empty() {
            "/".to_string()
        } else {
            r.mount
        };

        let id = self
            .registry
            .add_source(&r.session_id, &mount, r.layer, backend)
            .map_err(|e| Status::not_found(e))?;

        Ok(Response::new(SourceRef { id }))
    }

    type LaunchStream = Pin<Box<dyn Stream<Item = Result<LaunchEvent, Status>> + Send>>;

    async fn launch(
        &self,
        req: Request<LaunchReq>,
    ) -> Result<Response<Self::LaunchStream>, Status> {
        let r = req.into_inner();
        if r.exec.is_empty() {
            return Err(Status::invalid_argument("exec is required"));
        }
        let session_id = r.session_id;
        let opts = LaunchOpts {
            image: r.exec,
            args: r.args,
            wait: r.wait,
            shim_dll: None,
            payload_dll: None,
            env: r.env.into_iter().collect::<BTreeMap<_, _>>(),
        };

        let registry = self.registry.clone();
        let (tx, rx) = mpsc::channel::<Result<LaunchEvent, Status>>(4);

        tokio::task::spawn_blocking(move || {
            let _ = tx.blocking_send(Ok(LaunchEvent {
                event: Some(launch_event::Event::Started(
                    vfs_control::pb::Started { pid: 0 },
                )),
            }));

            match registry.launch(&session_id, opts) {
                Ok(code) => {
                    let _ = tx.blocking_send(Ok(LaunchEvent {
                        event: Some(launch_event::Event::Exited(vfs_control::pb::Exited {
                            code,
                        })),
                    }));
                }
                Err(e) => {
                    let _ = tx.blocking_send(Err(Status::internal(e)));
                }
            }
        });

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream) as Self::LaunchStream))
    }

    async fn teardown_session(
        &self,
        req: Request<TeardownReq>,
    ) -> Result<Response<Empty>, Status> {
        let id = req.into_inner().session_id;
        self.registry
            .teardown(&id)
            .map_err(|e| Status::not_found(e))?;
        Ok(Response::new(Empty {}))
    }

    async fn list_sessions(&self, _req: Request<Empty>) -> Result<Response<SessionList>, Status> {
        let sessions = self
            .registry
            .list()
            .map_err(|e| Status::internal(e))?
            .into_iter()
            .map(|s| Session {
                id: s.id,
                name: s.name,
                root: s.root.to_string_lossy().into_owned(),
            })
            .collect();
        Ok(Response::new(SessionList { sessions }))
    }

    async fn stats(&self, _req: Request<Empty>) -> Result<Response<StatsResp>, Status> {
        let s = self.registry.cache().stats();
        Ok(Response::new(StatsResp {
            cache_hits: s.hits,
            cache_misses: s.misses,
            cache_evicts: s.ram_evicts,
            cache_disk_hits: s.disk_hits,
            cache_bytes_from_cache: s.bytes_from_cache,
            cache_bytes_from_source: s.bytes_from_source,
            cache_ram_bytes: s.ram_bytes,
            sessions: self.registry.len() as u32,
        }))
    }
}

fn pb_to_source_spec(src: Option<&vfs_control::pb::SourceSpec>) -> Result<SourceSpec, String> {
    let src = src.ok_or_else(|| "source is required".to_string())?;
    match src.kind.as_ref() {
        Some(source_spec::Kind::Disk(d)) => Ok(SourceSpec::Disk {
            path: d.path.clone(),
        }),
        Some(source_spec::Kind::Zip(z)) => Ok(SourceSpec::Zip {
            path: z.path.clone(),
        }),
        Some(source_spec::Kind::Http(h)) => Ok(SourceSpec::Http {
            url: h.url.clone(),
        }),
        Some(source_spec::Kind::Remote(r)) => Ok(SourceSpec::Remote {
            endpoint: r.endpoint.clone(),
        }),
        None => Err("source.kind is required".into()),
    }
}
