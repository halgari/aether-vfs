//! The layer RPCs end to end, in process: a `DirectorService` over a
//! temporary `Storage`, driven through a gRPC client. Portable (no inject,
//! no Proton): AddSource with a layer, a write through the session kernel,
//! ExportLayer, DeleteLayer while the session is live, then ImportLayer,
//! ListLayers and DeleteLayer after teardown.

use std::sync::Arc;

use tokio::net::TcpListener;
use tonic::transport::Server;
use tonic::Code;
use vfs_control::pb::director_server::DirectorServer;
use vfs_control::pb::{
    source_spec, AddSourceReq, CreateSessionReq, DiskSource, Empty, LayerNameReq, LayerPathReq,
    LayerSource, TeardownReq,
};
use vfs_director::RootId;
use vfs_directord::{connect, DirectorService, SessionRegistry};
use vfs_embed::{Storage, StorageConfig, OPEN_CREATE, OPEN_WRITE};

fn layer_source(session_id: &str, name: &str, write_layer: bool) -> AddSourceReq {
    AddSourceReq {
        session_id: session_id.to_string(),
        source: Some(vfs_control::pb::SourceSpec {
            kind: Some(source_spec::Kind::Layer(LayerSource {
                name: name.to_string(),
            })),
        }),
        mount: "/".into(),
        layer: 0,
        root: 0,
        write_layer,
        cache_key: String::new(),
    }
}

async fn layer_names(
    client: &mut vfs_control::pb::director_client::DirectorClient<tonic::transport::Channel>,
) -> Vec<String> {
    let mut names: Vec<String> = client
        .list_layers(Empty {})
        .await
        .unwrap()
        .into_inner()
        .layers
        .into_iter()
        .map(|l| l.name)
        .collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread")]
async fn layer_rpcs_round_trip_through_a_session() {
    let store_dir = vfs_testkit::tempdir().unwrap();
    let storage = Storage::open(store_dir.path(), StorageConfig::default()).unwrap();
    let registry = SessionRegistry::with_storage(Arc::clone(&storage));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let svc = DirectorService::new(registry.clone());
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let mut client = connect(&format!("{addr}")).await.unwrap();

    let session = client
        .create_session(CreateSessionReq {
            name: "layers".into(),
        })
        .await
        .unwrap()
        .into_inner();
    let content = vfs_testkit::tempdir().unwrap();
    std::fs::write(content.path().join("base.txt"), b"base").unwrap();
    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(vfs_control::pb::SourceSpec {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: content.path().to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 0,
            root: 0,
            write_layer: false,
            cache_key: String::new(),
        })
        .await
        .unwrap();

    // A layer is only ever a write layer.
    let e = client
        .add_source(layer_source(&session.id, "prof", false))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument, "{e:?}");

    client
        .add_source(layer_source(&session.id, "prof", true))
        .await
        .unwrap();
    registry
        .with_session_mut(&session.id, |live| {
            let k = live.session.kernel();
            k.mkdir(RootId(0), "saves").unwrap();
            let (fh, _, _) = k
                .open(RootId(0), "saves/a.sav", OPEN_WRITE | OPEN_CREATE)
                .unwrap();
            assert_eq!(k.write(fh, 0, b"SAVED").unwrap(), 5);
            k.close(fh).unwrap();
            Ok(())
        })
        .unwrap();

    // Export while the session is live: the written file is there.
    let out = vfs_testkit::tempdir().unwrap();
    let export_dir = out.path().join("export");
    let n = client
        .export_layer(LayerPathReq {
            name: "prof".into(),
            dir: export_dir.to_string_lossy().into_owned(),
        })
        .await
        .unwrap()
        .into_inner()
        .files;
    assert_eq!(n, 1);
    assert_eq!(
        std::fs::read(export_dir.join("saves").join("a.sav")).unwrap(),
        b"SAVED"
    );

    // A layer a live session writes into cannot be deleted.
    let e = client
        .delete_layer(LayerNameReq {
            name: "prof".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::FailedPrecondition, "{e:?}");

    client
        .teardown_session(TeardownReq {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();

    // Import refuses an existing name, and creates a new one.
    let e = client
        .import_layer(LayerPathReq {
            name: "prof".into(),
            dir: export_dir.to_string_lossy().into_owned(),
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::AlreadyExists, "{e:?}");
    let n = client
        .import_layer(LayerPathReq {
            name: "copy".into(),
            dir: export_dir.to_string_lossy().into_owned(),
        })
        .await
        .unwrap()
        .into_inner()
        .files;
    assert_eq!(n, 1);
    assert_eq!(layer_names(&mut client).await, vec!["copy", "prof"]);

    client
        .delete_layer(LayerNameReq {
            name: "prof".into(),
        })
        .await
        .unwrap();
    assert_eq!(layer_names(&mut client).await, vec!["copy"]);

    server.abort();
}
