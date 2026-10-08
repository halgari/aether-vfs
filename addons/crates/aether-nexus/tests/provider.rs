//! `ArchiveProvider`: a repacked Nexus zip as a read-only provider tree,
//! against the fake file host (offline).
#![cfg(feature = "provider")]

#[path = "../../aether-net/tests/common/mod.rs"]
mod common;

use std::sync::Arc;

use aether_net::Events;
use aether_nexus::provider::ArchiveProvider;
use aether_nexus::{NexusArchive, NexusClient};
use common::repack::{RepackFile, data, repacked_zip};
use common::{API_KEY, TestServer, http, start};
use tokio::runtime::Runtime;
use vfs_provider::{
    FIXTURE_FILES, KIND_DIR, KIND_FILE, OPEN_READ, OPEN_WRITE, Provider, ST_NOT_A_DIRECTORY,
    ST_NOT_FOUND, VPath, assert_conformance,
};

const UID: u64 = (1704u64 << 32) + 4242;

/// A runtime driving the fake host, the host, and a provider over the
/// archive it serves. The provider is called from the test thread, which
/// is not a runtime worker (as a director worker would call it).
struct Fixture {
    rt: Runtime,
    server: TestServer,
    provider: ArchiveProvider,
}

fn fixture(files: Vec<RepackFile>) -> Fixture {
    let rt = Runtime::new().unwrap();
    let (server, archive) = rt.block_on(async {
        let server = start().await;
        server.put_repacked(UID, repacked_zip(&files));
        let client = Arc::new(
            NexusClient::new(http(Events::default()), API_KEY)
                .with_base_url(&server.base)
                .unwrap(),
        );
        let archive = NexusArchive::open(client, UID).await.unwrap();
        (server, archive)
    });
    let provider = ArchiveProvider::new(archive, rt.handle().clone());
    Fixture {
        rt,
        server,
        provider,
    }
}

fn at(rel: &str) -> VPath<'_> {
    VPath::at_default(rel)
}

#[test]
fn conformance() {
    let mut files: Vec<RepackFile> = vec![RepackFile::dir("sub/")];
    files.extend(
        FIXTURE_FILES
            .iter()
            .map(|(name, body)| RepackFile::new(name, body.to_vec())),
    );
    let f = fixture(files);
    let signed = f.server.requests("/v3/").len();
    assert_eq!(signed, 1, "the open signed one link");

    let p: Arc<dyn Provider> = Arc::new(f.provider);
    let caps = p.capabilities();
    assert!(caps.immutable && caps.slow, "{caps:?}");
    assert_conformance(p.clone());

    // Reads reuse the link the open signed: the provider signs none ahead
    // and none of its own.
    assert_eq!(f.server.requests("/v3/").len(), signed);
    assert_eq!(
        p.open(at("a.txt"), OPEN_READ | OPEN_WRITE).err(),
        Some(vfs_provider::ST_READ_ONLY)
    );
}

#[test]
fn building_the_provider_makes_no_request() {
    let f = fixture(vec![RepackFile::new("Plugin.esp", data(100, 1))]);
    f.server.clear_log();
    let p = &f.provider;
    let (h, size, is_dir) = p.open(at("plugin.esp"), OPEN_READ).unwrap();
    assert_eq!((size, is_dir), (100, false));
    p.getattr(at("Plugin.esp")).unwrap().unwrap();
    p.readdir(at("")).unwrap();
    assert!(
        f.server.requests("/").is_empty(),
        "lookups and opens are answered from the index"
    );
    p.close(h).unwrap();
}

#[test]
fn case_insensitive_lookup() {
    let f = fixture(vec![
        RepackFile::new("Data/Meshes/Iron.NIF", data(300, 1)),
        // A second spelling of the same directory: the first one names it.
        RepackFile::new("DATA/meshes/Steel.nif", data(200, 2)),
        RepackFile::new(r"Interface\Translations\Mod_English.txt", data(50, 3)),
        RepackFile::new("Data/ÜBER/a.esp", data(10, 4)),
    ]);
    let p = &f.provider;

    for q in [
        "data/meshes/iron.nif",
        "DATA/MESHES/IRON.NIF",
        "Data/Meshes/Iron.NIF",
    ] {
        let st = p.getattr(at(q)).unwrap().unwrap_or_else(|| panic!("{q}"));
        assert_eq!((st.kind, st.size), (KIND_FILE, 300), "{q}");
    }
    let st = p.getattr(at("data/über/A.ESP")).unwrap().unwrap();
    assert_eq!(st.size, 10);
    assert_eq!(
        p.getattr(at("interface/translations/mod_english.TXT"))
            .unwrap()
            .unwrap()
            .size,
        50,
        "backslashes in the zip are separators"
    );
    assert_eq!(p.getattr(at("data/meshes/nope.nif")).unwrap(), None);

    // Listings show the archive's spelling, each name once.
    let names = |q: &str| -> Vec<String> {
        p.readdir(at(q))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect()
    };
    let mut root = names("");
    root.sort();
    assert_eq!(root, ["Data", "Interface"]);
    let mut data_dir = names("DATA");
    data_dir.sort();
    assert_eq!(data_dir, ["Meshes", "ÜBER"]);
    let mut meshes = names("data/MESHES");
    meshes.sort();
    assert_eq!(meshes, ["Iron.NIF", "Steel.nif"]);
    let kinds: Vec<u8> = p
        .readdir(at("data"))
        .unwrap()
        .iter()
        .map(|e| e.stat.kind)
        .collect();
    assert!(kinds.iter().all(|&k| k == KIND_DIR), "{kinds:?}");
    assert_eq!(
        p.stored_name(at("data/meshes/steel.NIF"))
            .unwrap()
            .as_deref(),
        Some("Steel.nif")
    );
    assert_eq!(
        p.stored_name(at("data/meshes")).unwrap().as_deref(),
        Some("Meshes")
    );
    assert_eq!(
        p.readdir(at("data/meshes/iron.nif")).err(),
        Some(ST_NOT_A_DIRECTORY)
    );
    assert_eq!(p.readdir(at("nope")).err(), Some(ST_NOT_FOUND));

    // Opened through another spelling, the bytes are the entry's.
    let (h, size, _) = p.open(at("dAtA/mEsHeS/sTeEl.NiF"), OPEN_READ).unwrap();
    let mut buf = vec![0u8; size as usize];
    assert_eq!(p.read_at(h, 0, &mut buf).unwrap(), 200);
    assert_eq!(buf, data(200, 2));
    p.close(h).unwrap();
}

#[test]
fn read_spans_frames_and_eof() {
    const FRAME: usize = 64 << 10;
    let big = data(200_000, 7);
    let f = fixture(vec![
        RepackFile::new("Textures/Big.dds", big.clone()).frames(FRAME),
    ]);
    let p = &f.provider;
    let (h, size, is_dir) = p.open(at("textures/big.dds"), OPEN_READ).unwrap();
    assert_eq!((size, is_dir), (big.len() as u64, false));

    // Starts in frame 0, ends in frame 2.
    let from = FRAME - 1000;
    let mut buf = vec![0u8; FRAME + 2000];
    let n = p.read_at(h, from as u64, &mut buf).unwrap();
    assert_eq!(n, buf.len());
    assert_eq!(buf, big[from..from + n]);

    // A read running past the end is short; at or past the end it is 0.
    let mut buf = vec![0u8; 500];
    let n = p.read_at(h, size - 100, &mut buf).unwrap();
    assert_eq!(n, 100);
    assert_eq!(buf[..n], big[big.len() - 100..]);
    assert_eq!(p.read_at(h, size, &mut buf).unwrap(), 0);
    assert_eq!(p.read_at(h, size + 4096, &mut buf).unwrap(), 0);
    assert_eq!(p.read_at(h, 0, &mut []).unwrap(), 0);

    // The whole file in one read.
    let mut all = vec![0u8; big.len()];
    assert_eq!(p.read_at(h, 0, &mut all).unwrap(), big.len());
    assert_eq!(all, big);

    p.close(h).unwrap();
    assert!(p.read_at(h, 0, &mut buf).is_err(), "closed handle");
}

/// Called on a runtime worker, the provider refuses instead of blocking
/// the worker its own read needs.
#[test]
fn refuses_to_block_a_runtime_worker() {
    let f = fixture(vec![RepackFile::new("a.esp", data(100, 1))]);
    let (h, _, _) = f.provider.open(at("a.esp"), OPEN_READ).unwrap();
    let p = Arc::new(f.provider);
    let p2 = p.clone();
    let r = f.rt.block_on(async move {
        tokio::spawn(async move { p2.read_at(h, 0, &mut [0u8; 10]) })
            .await
            .unwrap()
    });
    assert_eq!(r, Err(vfs_provider::ST_IO_ERROR));
    // From a blocking-pool thread it is fine.
    let p3 = p.clone();
    let r = f.rt.block_on(async move {
        tokio::task::spawn_blocking(move || p3.read_at(h, 0, &mut [0u8; 10]))
            .await
            .unwrap()
    });
    assert_eq!(r, Ok(10));
}
