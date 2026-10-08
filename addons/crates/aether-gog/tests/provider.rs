//! `DepotProvider`: GOG depots as one read-only `vfs_provider::Provider`
//! tree, read from the fake CDN.
mod fake_gog;

use std::sync::Arc;

use aether_gog::provider::DepotProvider;
use aether_gog::{
    Chunk, DepotItem, DepotManifest, GogContent, Os, ProductId, SfcRef, complete_login,
};
use fake_gog::{CHUNK, CODE, FakeGog, GAME, big, http, small_ini, start};
use tokio::runtime::Runtime;
use vfs_provider::{
    FIXTURE_FILES, KIND_DIR, KIND_FILE, OPEN_READ, OPEN_WRITE, Provider, ST_NOT_A_DIRECTORY, VPath,
    assert_conformance,
};

/// A multi-thread runtime running the fake, a login against it, and a
/// temporary cache directory. Provider calls are made from the test
/// thread, never a runtime worker.
struct Env {
    rt: Runtime,
    fake: FakeGog,
    content: GogContent,
    _dir: tempfile::TempDir,
}

fn env() -> Env {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (fake, content) = rt.block_on(async {
        let fake = start().await;
        let cfg = fake.config(dir.path());
        complete_login(&http(), &cfg, CODE).await.unwrap();
        let content = GogContent::open(http(), cfg).await.unwrap();
        (fake, content)
    });
    Env {
        rt,
        fake,
        content,
        _dir: dir,
    }
}

impl Env {
    fn provider(&self, depots: Vec<DepotManifest>) -> DepotProvider {
        DepotProvider::new(
            self.content.clone(),
            depots
                .into_iter()
                .map(|m| (ProductId(GAME), Arc::new(m)))
                .collect(),
            self.rt.handle().clone(),
        )
    }

    /// The fixture build's depots (`Data\Big.bin`, `Readme.TXT`,
    /// `Data\Small.ini` in the small-files container; the DLC's
    /// `Data\DLC.esp`).
    fn fixture_depots(&self) -> Vec<(ProductId, Arc<DepotManifest>)> {
        self.rt.block_on(async {
            let builds = self
                .content
                .builds(ProductId(GAME), Os::Windows)
                .await
                .unwrap();
            let details = self.content.build_details(&builds[0]).await.unwrap();
            let mut out = Vec::new();
            for d in &details.depots {
                out.push((d.product_id, self.content.depot(d).await.unwrap()));
            }
            out
        })
    }
}

fn read(p: &dyn Provider, path: &str) -> Vec<u8> {
    let (h, size, is_dir) = p.open(VPath::at_default(path), OPEN_READ).unwrap();
    assert!(!is_dir, "{path}");
    let mut buf = vec![0u8; size as usize];
    let mut at = 0;
    while at < buf.len() {
        let n = p.read_at(h, at as u64, &mut buf[at..]).unwrap();
        assert!(n > 0, "{path}: short read at {at}");
        at += n;
    }
    p.close(h).unwrap();
    buf
}

fn names(p: &dyn Provider, dir: &str) -> Vec<String> {
    let mut v: Vec<String> = p
        .readdir(VPath::at_default(dir))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    v.sort();
    v
}

#[test]
fn conformance() {
    let env = env();
    // Depot 0: `a.txt` in a small-files container (bytes 2..7); depot 1:
    // `sub\b.txt` across two chunks.
    let (a, a_body) = FIXTURE_FILES[0];
    let (b, b_body) = FIXTURE_FILES[1];
    let container = [b"..".as_slice(), a_body, b"..."].concat();
    let sfc_chunk: Chunk = env.fake.add_chunk(&container);
    let depot0 = DepotManifest {
        items: vec![DepotItem {
            path: a.to_string(),
            chunks: Vec::new(),
            size: a_body.len() as u64,
            md5: None,
            sfc_ref: Some(SfcRef {
                offset: 2,
                size: a_body.len() as u64,
            }),
            flags: Vec::new(),
        }],
        small_files_container: Some(vec![sfc_chunk]),
    };
    let b_win = b.replace('/', "\\");
    let depot1 = env
        .fake
        .manifest(&[(b_win.as_str(), &[&b_body[..3], &b_body[3..]])]);
    let p = env.provider(vec![depot0, depot1]);
    assert_conformance(Arc::new(p));
}

#[test]
fn case_insensitive_lookup() {
    let env = env();
    let depot = env.fake.manifest(&[
        ("Data\\Skyrim.esm", &[b"TES4 master"]),
        ("Data\\Über\\Straße.ini", &[b"[General]"]),
    ]);
    let p = env.provider(vec![depot]);
    let st = p
        .getattr(VPath::at_default("data/SKYRIM.ESM"))
        .unwrap()
        .expect("found in any case");
    assert_eq!((st.kind, st.size), (KIND_FILE, 11));
    assert!(
        p.getattr(VPath::at_default("DATA\\skyrim.esm"))
            .unwrap()
            .is_some()
    );
    assert_eq!(
        p.getattr(VPath::at_default("dAtA")).unwrap().unwrap().kind,
        KIND_DIR
    );
    // The manifest's spelling in listings and stored names.
    assert_eq!(names(&p, "DATA"), ["Skyrim.esm", "Über"]);
    assert_eq!(names(&p, ""), ["Data"]);
    assert_eq!(names(&p, "data/über"), ["Straße.ini"]);
    assert_eq!(
        p.stored_name(VPath::at_default("data/skyrim.ESM"))
            .unwrap()
            .as_deref(),
        Some("Skyrim.esm")
    );
    assert_eq!(
        p.stored_name(VPath::at_default("DATA/ÜBER"))
            .unwrap()
            .as_deref(),
        Some("Über")
    );
    assert_eq!(read(&p, "data/ÜBER/STRAßE.INI"), b"[General]");
    assert_eq!(read(&p, "DATA/skyrim.ESM"), b"TES4 master");
    // A file is not a directory; nothing is writable.
    assert_eq!(
        p.readdir(VPath::at_default("data/skyrim.esm")),
        Err(ST_NOT_A_DIRECTORY)
    );
    assert!(
        p.open(VPath::at_default("data/skyrim.esm"), OPEN_WRITE)
            .is_err()
    );
    assert!(
        p.getattr(VPath::at_default("data/nope.esm"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn earlier_depot_wins() {
    let env = env();
    let first = env
        .fake
        .manifest(&[("a.txt", &[b"first depot"]), ("Data\\One.esp", &[b"one"])]);
    let second = env.fake.manifest(&[
        ("A.TXT", &[b"second"]),
        ("data\\Two.esp", &[b"two"]),
        ("DATA\\one.ESP", &[b"not one"]),
    ]);
    let p = env.provider(vec![first, second]);
    assert_eq!(read(&p, "a.txt"), b"first depot");
    assert_eq!(
        p.getattr(VPath::at_default("A.txt")).unwrap().unwrap().size,
        11
    );
    assert_eq!(names(&p, ""), ["Data", "a.txt"]);
    // Directories merge, spelled as the first depot has them.
    assert_eq!(names(&p, "data"), ["One.esp", "Two.esp"]);
    assert_eq!(read(&p, "data/one.esp"), b"one");
    assert_eq!(read(&p, "data/two.esp"), b"two");
}

#[test]
fn read_spans_chunks_and_eof() {
    let env = env();
    let p = DepotProvider::new(
        env.content.clone(),
        env.fixture_depots(),
        env.rt.handle().clone(),
    );
    let data = big();
    let (h, size, _) = p
        .open(VPath::at_default("data/big.bin"), OPEN_READ)
        .unwrap();
    assert_eq!(size, data.len() as u64);
    // Across the boundary of chunks 0 and 1.
    let mut buf = [0u8; 200];
    assert_eq!(p.read_at(h, CHUNK as u64 - 100, &mut buf).unwrap(), 200);
    assert_eq!(buf[..], data[CHUNK - 100..CHUNK + 100]);
    // Short at the end, zero at and past it.
    assert_eq!(p.read_at(h, size - 5, &mut buf).unwrap(), 5);
    assert_eq!(buf[..5], data[data.len() - 5..]);
    assert_eq!(p.read_at(h, size, &mut buf).unwrap(), 0);
    assert_eq!(p.read_at(h, size + 1000, &mut buf).unwrap(), 0);
    p.close(h).unwrap();
    assert_eq!(read(&p, "Data\\Big.bin"), data);
    // A small-files-container entry, and the DLC depot's file.
    assert_eq!(read(&p, "DATA/small.INI"), small_ini());
    assert_eq!(read(&p, "data/dlc.esp"), fake_gog::dlc_esp());
    assert_eq!(names(&p, "data"), ["Big.bin", "DLC.esp", "Small.ini"]);

    // On a runtime worker a read fails rather than blocking the runtime.
    let p = Arc::new(p);
    let r = env.rt.block_on(async move {
        tokio::spawn(async move {
            let (h, _, _) = p.open(VPath::at_default("readme.txt"), OPEN_READ)?;
            p.read_at(h, 0, &mut [0u8; 4])
        })
        .await
        .unwrap()
    });
    assert!(r.is_err());
}
