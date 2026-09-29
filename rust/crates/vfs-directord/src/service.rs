//! tonic [`Director`] service implementation.

use std::collections::BTreeMap;
use std::pin::Pin;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

use vfs_control::pb::director_server::Director;
use vfs_control::pb::{
    launch_event, source_spec, AddSourceReq, CreateSessionReq, DeclareRootReq, Empty, HealthReq,
    HealthResp, LaunchEvent, LaunchReq, LayerCount, LayerInfo, LayerList, LayerNameReq,
    LayerPathReq, RejectedWrite, Session, SessionList, SourceRef, StatsResp, TeardownReq,
};
use vfs_control::SourceSpec;
use vfs_embed::{open_totals, rejected_writes, LaunchOpts, SourceKey, Storage, StorageError};
use vfs_source::build_provider;

use crate::registry::{LayerOpError, SessionRegistry, NO_STORAGE};

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
        let summary = self.registry.create(name).map_err(create_status)?;
        Ok(Response::new(Session {
            id: summary.id,
            name: summary.name,
            root: summary.root.to_string_lossy().into_owned(),
        }))
    }

    async fn declare_root(&self, req: Request<DeclareRootReq>) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        if r.path.trim().is_empty() {
            return Err(Status::invalid_argument("path is required"));
        }
        self.registry
            .declare_root(
                &r.session_id,
                r.root,
                std::path::Path::new(&r.path),
                &r.name,
            )
            .map_err(Status::invalid_argument)?;
        Ok(Response::new(Empty {}))
    }

    async fn add_source(&self, req: Request<AddSourceReq>) -> Result<Response<SourceRef>, Status> {
        let r = req.into_inner();
        let spec = pb_to_source_spec(r.source.as_ref()).map_err(Status::invalid_argument)?;
        let mount = if r.mount.is_empty() {
            "/".to_string()
        } else {
            r.mount
        };

        // A storage layer is only ever a write layer (spec §7: valid only
        // with `write_layer = true`, which `validate_roots` enforces for
        // configs; this is the RPC's own check). It lives in the daemon's
        // storage, not in anything `build_provider` can open.
        if let SourceSpec::Layer { name } = &spec {
            if !r.write_layer {
                return Err(Status::invalid_argument(format!(
                    "layer source {name:?} must be the write layer (write_layer = true)"
                )));
            }
            check_whole_root(&mount)?;
            let (registry, sid, name) = (self.registry.clone(), r.session_id, name.clone());
            // Opening a layer can wait for its previous provider's final
            // commit: off the async executor.
            let id = tokio::task::spawn_blocking(move || {
                registry.set_layer_write_layer(&sid, r.root, &name)
            })
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(|e| {
                if e.contains(NO_STORAGE) {
                    Status::failed_precondition(e)
                } else {
                    registry_status(e)
                }
            })?;
            return Ok(Response::new(SourceRef { id }));
        }

        // The cache identity: the config's `cache_key`, else what names the
        // source — the remote endpoint (only an immutable, slow source is
        // cached at all, so for a disk or zip source the path is never used).
        let key = SourceKey(if r.cache_key.is_empty() {
            spec_identity(&spec)
        } else {
            r.cache_key.clone()
        });

        // build_provider may block (remote connect); run off the async executor.
        let backend = tokio::task::spawn_blocking(move || build_provider(&spec))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        // A write layer is not a sibling source, so it does not go through
        // `add_source` at all — it becomes the root's overlay upper, which is
        // what makes an in-place edit of read-only content copy up instead of
        // being refused. A sub-path mount cannot express that (the upper
        // covers the whole root), and a read-only provider cannot be one, so
        // both are rejected here rather than accepted into a session that
        // then silently lacks copy-on-write.
        if r.write_layer {
            check_whole_root(&mount)?;
            // Replacing a named-layer upper drops its provider, whose `Drop`
            // runs a durable point (fsyncs): off the async executor.
            let (registry, sid) = (self.registry.clone(), r.session_id);
            let id = tokio::task::spawn_blocking(move || {
                registry.set_write_layer(&sid, r.root, backend)
            })
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(registry_status)?;
            return Ok(Response::new(SourceRef { id }));
        }

        let id = self
            .registry
            .add_source_keyed(&r.session_id, r.root, &mount, r.layer, backend, key)
            .map_err(registry_status)?;

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
        // `LaunchReq.session_id` may name the session (`vfs exec --session
        // NAME`), and `exec` may start with a root's `{Name}`; both are this
        // host's vocabulary, so both are resolved here before `Session` sees
        // an id and a path.
        let session_id = self
            .registry
            .resolve_session(&r.session_id)
            .map_err(Status::not_found)?;
        let exec = self
            .registry
            .expand_root_name(&session_id, &r.exec)
            .map_err(Status::invalid_argument)?;
        let opts = LaunchOpts {
            image: exec,
            args: r.args,
            wait: r.wait,
            shim_dll: None,
            payload_dll: None,
            env: r.env.into_iter().collect::<BTreeMap<_, _>>(),
            // `LaunchReq` carries no launcher/spawn-target chain and no
            // redistributable fallback dirs: a generic RPC launch has no
            // game-specific knowledge to put there. A caller that needs
            // either stages before launching.
            ..Default::default()
        };

        let registry = self.registry.clone();
        let (tx, rx) = mpsc::channel::<Result<LaunchEvent, Status>>(4);

        tokio::task::spawn_blocking(move || {
            let _ = tx.blocking_send(Ok(LaunchEvent {
                event: Some(launch_event::Event::Started(vfs_control::pb::Started {
                    pid: 0,
                })),
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

    async fn teardown_session(&self, req: Request<TeardownReq>) -> Result<Response<Empty>, Status> {
        // By id or by name, like `Launch` — `vfs down --session NAME`.
        let id = self
            .registry
            .resolve_session(&req.into_inner().session_id)
            .map_err(Status::not_found)?;
        // Dropping the session can block for seconds on Linux (stopping a
        // prefix's `wineserver`, deleting an anonymous prefix): off the
        // async executor.
        let registry = self.registry.clone();
        tokio::task::spawn_blocking(move || registry.teardown(&id))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(|e| {
                if e.starts_with("unknown session") {
                    Status::not_found(e)
                } else {
                    Status::failed_precondition(e)
                }
            })?;
        Ok(Response::new(Empty {}))
    }

    async fn list_sessions(&self, _req: Request<Empty>) -> Result<Response<SessionList>, Status> {
        let sessions = self
            .registry
            .list()
            .map_err(Status::internal)?
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
        // Spec §3's mapping. Without storage every storage figure is zero.
        let s = match self.registry.storage().cloned() {
            Some(storage) => tokio::task::spawn_blocking(move || storage.stats())
                .await
                .map_err(|e| Status::internal(e.to_string()))?,
            None => Default::default(),
        };
        // Director-side half of the shim/director open-count reconciliation
        // (aether-vfs measurement gate): the shim classifies every under-root
        // open by which path it took; these are the opens that actually
        // arrived here. Both counters are process-wide, not per-session, same
        // as the storage metrics above — see `vfs_embed::open_totals`.
        let (opens_ok, opens_err) = open_totals();
        let rejected_writes = rejected_writes()
            .into_iter()
            .map(|(path, count)| RejectedWrite { path, count })
            .collect();
        Ok(Response::new(StatsResp {
            cache_hits: s.cache.hits,
            cache_misses: s.cache.misses,
            cache_evicts: s.cache.ram_evicts,
            cache_disk_hits: s.cache.store_hits,
            cache_bytes_from_cache: s.cache.bytes_from_cache,
            cache_bytes_from_source: s.cache.bytes_from_source,
            cache_ram_bytes: s.cache.ram_bytes,
            sessions: self.registry.len() as u32,
            opens_ok,
            opens_err,
            rejected_writes,
            store_pack_bytes: s.pack_bytes,
            store_live_bytes: s.live_bytes,
            cache_logical_bytes: s.cache.cached_logical_bytes,
            layers: u32::try_from(s.layer_count).unwrap_or(u32::MAX),
        }))
    }

    async fn list_layers(&self, _req: Request<Empty>) -> Result<Response<LayerList>, Status> {
        let storage = self.storage()?;
        let layers = tokio::task::spawn_blocking(move || storage.layers())
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(storage_status)?
            .into_iter()
            .map(|l| LayerInfo {
                name: l.name,
                files: l.files,
                logical_bytes: l.logical_bytes,
            })
            .collect();
        Ok(Response::new(LayerList { layers }))
    }

    async fn export_layer(
        &self,
        req: Request<LayerPathReq>,
    ) -> Result<Response<LayerCount>, Status> {
        let r = req.into_inner();
        let (name, dir) = layer_path_req(&r)?;
        let storage = self.storage()?;
        let files = tokio::task::spawn_blocking(move || storage.export_layer(&name, &dir))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(storage_status)?;
        Ok(Response::new(LayerCount { files }))
    }

    async fn import_layer(
        &self,
        req: Request<LayerPathReq>,
    ) -> Result<Response<LayerCount>, Status> {
        let r = req.into_inner();
        let (name, dir) = layer_path_req(&r)?;
        let storage = self.storage()?;
        let files = tokio::task::spawn_blocking(move || storage.import_layer(&dir, &name))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(storage_status)?;
        Ok(Response::new(LayerCount { files }))
    }

    async fn delete_layer(&self, req: Request<LayerNameReq>) -> Result<Response<Empty>, Status> {
        let name = req.into_inner().name;
        if name.is_empty() {
            return Err(Status::invalid_argument("a layer name is required"));
        }
        let registry = self.registry.clone();
        tokio::task::spawn_blocking(move || registry.delete_layer(&name))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(|e| match e {
                LayerOpError::NoStorage | LayerOpError::InUse(_) => {
                    Status::failed_precondition(e.to_string())
                }
                LayerOpError::Storage(e) => storage_status(e),
            })?;
        Ok(Response::new(Empty {}))
    }
}

impl DirectorService {
    /// The daemon's storage, or `FailedPrecondition` naming how to get one.
    fn storage(&self) -> Result<std::sync::Arc<Storage>, Status> {
        self.registry
            .storage()
            .cloned()
            .ok_or_else(|| Status::failed_precondition(NO_STORAGE))
    }
}

/// A write layer is the root's writable upper: it covers the whole root.
fn check_whole_root(mount: &str) -> Result<(), Status> {
    if mount == "/" || mount == "\\" {
        Ok(())
    } else {
        Err(Status::invalid_argument(format!(
            "write_layer source mounts at {mount:?}; a write layer is the root's \
             writable upper and cannot be scoped to a sub-path"
        )))
    }
}

/// What names a source's content when its config sets no `cache_key`.
fn spec_identity(spec: &SourceSpec) -> String {
    match spec {
        SourceSpec::Remote { endpoint } => endpoint.clone(),
        SourceSpec::Http { url } => url.clone(),
        SourceSpec::Disk { path } | SourceSpec::Zip { path } => path.clone(),
        SourceSpec::Layer { name } => name.clone(),
        SourceSpec::Memory { .. } => String::new(),
    }
}

/// `LayerPathReq`'s two fields, both required.
fn layer_path_req(r: &LayerPathReq) -> Result<(String, std::path::PathBuf), Status> {
    if r.name.is_empty() {
        return Err(Status::invalid_argument("a layer name is required"));
    }
    if r.dir.is_empty() {
        return Err(Status::invalid_argument("a directory is required"));
    }
    Ok((r.name.clone(), std::path::PathBuf::from(&r.dir)))
}

/// A storage error as a gRPC status.
fn storage_status(e: StorageError) -> Status {
    let msg = e.to_string();
    match e {
        StorageError::NoSuchLayer(_) | StorageError::NotFound(_) => Status::not_found(msg),
        StorageError::LayerExists(_) => Status::already_exists(msg),
        StorageError::LayerInUse(_) => Status::failed_precondition(msg),
        StorageError::Exists(_) | StorageError::BadRequest(_) | StorageError::NotEmpty(_) => {
            Status::invalid_argument(msg)
        }
        StorageError::Io(ref io) if io.kind() == std::io::ErrorKind::NotFound => {
            Status::not_found(msg)
        }
        _ => Status::internal(msg),
    }
}

/// Map a `SessionRegistry` error string onto a gRPC status.
///
/// The registry answers in prose, and the two failures behind `AddSource`
/// are genuinely different: naming a session that does not exist is
/// `NotFound`, while a request the session refuses (a read-only write layer,
/// say) is `InvalidArgument`. Deciding that per call site let the same
/// "unknown session" answer come back as `NotFound` for a source and
/// `InvalidArgument` for a write layer — one function so the two agree.
fn registry_status(err: String) -> Status {
    if err.starts_with("unknown session") {
        Status::not_found(err)
    } else {
        Status::invalid_argument(err)
    }
}

/// Map a `SessionRegistry::create` refusal onto a gRPC status: a name
/// already live is `AlreadyExists`; a name that cannot name a Wine prefix is
/// the caller's to fix (`InvalidArgument`); anything else is the daemon's.
fn create_status(err: String) -> Status {
    if err.starts_with(crate::registry::DUPLICATE_NAME) {
        Status::already_exists(err)
    } else if err.contains("cannot name a Wine prefix") {
        Status::invalid_argument(err)
    } else {
        Status::internal(err)
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
        Some(source_spec::Kind::Http(h)) => Ok(SourceSpec::Http { url: h.url.clone() }),
        Some(source_spec::Kind::Remote(r)) => Ok(SourceSpec::Remote {
            endpoint: r.endpoint.clone(),
        }),
        Some(source_spec::Kind::Layer(l)) => Ok(SourceSpec::Layer {
            name: l.name.clone(),
        }),
        None => Err("source.kind is required".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    async fn stats(reg: &SessionRegistry) -> StatsResp {
        DirectorService::new(reg.clone())
            .stats(Request::new(Empty {}))
            .await
            .unwrap()
            .into_inner()
    }

    /// `Stats` reports `Storage::stats()` through spec §3's field mapping,
    /// and zeros for the storage fields without storage.
    #[tokio::test]
    async fn stats_report_storage() {
        let none = stats(&SessionRegistry::new()).await;
        assert_eq!(
            (
                none.store_pack_bytes,
                none.store_live_bytes,
                none.cache_logical_bytes,
                none.layers
            ),
            (0, 0, 0, 0)
        );

        let dir = tempfile::tempdir().unwrap();
        let storage =
            vfs_embed::Storage::open(dir.path(), vfs_embed::StorageConfig::default()).unwrap();
        let reg = SessionRegistry::with_storage(Arc::clone(&storage));
        let s = reg.create("stats".into()).unwrap();
        reg.set_layer_write_layer(&s.id, 0, "one").unwrap();
        reg.with_session_mut(&s.id, |live| {
            let k = live.session.kernel();
            let (fh, _, _) = k
                .open(
                    vfs_embed::RootId(0),
                    "f.bin",
                    vfs_embed::OPEN_WRITE | vfs_embed::OPEN_CREATE,
                )
                .unwrap();
            k.write(fh, 0, &vec![9u8; 300_000]).unwrap();
            k.flush(fh).unwrap();
            k.close(fh).unwrap();
            Ok(())
        })
        .unwrap();
        reg.teardown(&s.id).unwrap();

        let want = storage.stats();
        let got = stats(&reg).await;
        assert_eq!(got.layers, 1);
        assert!(
            got.store_pack_bytes > 0 && got.store_live_bytes > 0,
            "{got:?}"
        );
        assert_eq!(got.store_pack_bytes, want.pack_bytes);
        assert_eq!(got.store_live_bytes, want.live_bytes);
        assert_eq!(got.cache_logical_bytes, want.cache.cached_logical_bytes);
        assert_eq!(got.cache_hits, want.cache.hits);
        assert_eq!(got.cache_misses, want.cache.misses);
        assert_eq!(got.cache_evicts, want.cache.ram_evicts);
        assert_eq!(got.cache_disk_hits, want.cache.store_hits);
        assert_eq!(got.cache_bytes_from_cache, want.cache.bytes_from_cache);
        assert_eq!(got.cache_bytes_from_source, want.cache.bytes_from_source);
        assert_eq!(got.cache_ram_bytes, want.cache.ram_bytes);
    }
}
