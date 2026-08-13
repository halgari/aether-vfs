//! Sources ("filesystem shards"). Every source implements the [`Provider`] op
//! contract; [`build_provider`] turns a declarative [`SourceSpec`] into a live
//! provider. Out-of-process plugins speak gRPC [`SourceService`](pb).

mod remote;
mod rt;
mod serve;

pub mod pb {
    tonic::include_proto!("vfs.source");
}

pub use remote::RemoteProvider;
pub use serve::ProviderSourceService;
pub use vfs_control::SourceSpec;
pub use vfs_provider::{assert_conformance, write_fixture_tree};
pub use vfs_provider::{DirEntry, Provider, Stat, KIND_DIR, KIND_FILE, OPEN_READ};

use std::sync::Arc;

/// Wire contract version for the out-of-process `Source` gRPC service.
/// `RemoteProvider::connect` rejects any server that reports a different
/// value, and `ProviderSourceService` reports exactly this value — the two
/// sides must agree on the wire shape (`source.proto`) before any op is
/// exchanged. Bump when that shape changes.
pub const SOURCE_CONTRACT_VERSION: u32 = 1;

#[derive(Debug)]
pub enum BuildError {
    Unsupported(&'static str),
    Open(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::Unsupported(m) => write!(f, "unsupported source: {m}"),
            BuildError::Open(m) => write!(f, "open source: {m}"),
        }
    }
}
impl std::error::Error for BuildError {}

/// Build a live provider from a declarative spec.
pub fn build_provider(spec: &SourceSpec) -> Result<Arc<dyn Provider>, BuildError> {
    match spec {
        SourceSpec::Disk { path } => Ok(Arc::new(vfs_director::DiskProvider::new(path))),
        SourceSpec::Zip { path } => {
            let p = vfs_zip::ZipProvider::open(std::path::Path::new(path))
                .map_err(|e| BuildError::Open(format!("{path}: {e:?}")))?;
            Ok(Arc::new(p))
        }
        SourceSpec::Http { .. } => Err(BuildError::Unsupported("http source (later milestone)")),
        SourceSpec::Remote { endpoint } => {
            let p = RemoteProvider::connect_blocking(endpoint)
                .map_err(BuildError::Open)?;
            Ok(Arc::new(p))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tonic::transport::Server;
    use vfs_director::DiskProvider;

    #[test]
    fn builds_disk_provider() {
        let dir = std::env::temp_dir().join(format!("vfs-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), b"hi").unwrap();
        let p = build_provider(&SourceSpec::Disk {
            path: dir.to_string_lossy().into_owned(),
        })
        .unwrap();
        let st = p
            .getattr(vfs_provider::VPath::at_default("f.txt"))
            .unwrap()
            .unwrap();
        assert_eq!(st.size, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_conformance() {
        let dir = std::env::temp_dir().join(format!("vfs-conf-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);
        let p: Arc<dyn Provider> = Arc::new(DiskProvider::new(&dir));
        vfs_provider::assert_conformance(p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_provider_conformance_and_capabilities() {
        let dir = std::env::temp_dir().join(format!("vfs-rconf-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);
        let p: Arc<dyn Provider> = Arc::new(DiskProvider::new(&dir));
        let svc = ProviderSourceService::new(p);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(crate::pb::source_server::SourceServer::new(svc))
                .serve_with_incoming(incoming)
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let remote = RemoteProvider::connect(&format!("{addr}")).await.unwrap();
        let remote: Arc<dyn Provider> = Arc::new(remote);

        // Capabilities must survive the round trip, not be defaulted locally.
        assert_eq!(remote.capabilities().access, vfs_provider::Access::Read);
        assert!(!remote.capabilities().immutable, "disk is mutable, and the wire must say so");

        // The service wraps a ReadWrite DiskProvider, but the Stage-1 wire
        // contract has no write RPCs — so what crosses the wire must be Read.
        assert_eq!(
            remote.capabilities().access,
            vfs_provider::Access::Read,
            "the gRPC service must clamp access to what the wire can serve"
        );

        tokio::task::spawn_blocking(move || {
            vfs_provider::assert_conformance(remote);
        })
        .await
        .unwrap();

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn http_still_unsupported() {
        assert!(matches!(
            build_provider(&SourceSpec::Http { url: "http://x".into() }),
            Err(BuildError::Unsupported(_))
        ));
    }

    #[test]
    fn builds_zip_provider() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("vfs-zip-src-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // Minimal Stored zip with entry "f.txt" = "hi"
        let content = b"hi";
        let name = "f.txt";
        let mut buf = Vec::new();
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in content {
            crc ^= b as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        crc = !crc;
        let n = name.len() as u16;
        buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 4]);
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
        buf.extend_from_slice(&n.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(content);
        let cd_start = buf.len() as u32;
        buf.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 6]);
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
        buf.extend_from_slice(&n.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(name.as_bytes());
        let cd_size = buf.len() as u32 - cd_start;
        buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 4]);
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&cd_size.to_le_bytes());
        buf.extend_from_slice(&cd_start.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        let zip_path = dir.join("t.zip");
        std::fs::File::create(&zip_path).unwrap().write_all(&buf).unwrap();

        let p = build_provider(&SourceSpec::Zip {
            path: zip_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let st = p
            .getattr(vfs_provider::VPath::at_default("f.txt"))
            .unwrap()
            .unwrap();
        assert_eq!(st.size, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
