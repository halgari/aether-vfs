//! Sources ("filesystem shards"). Every source implements the [`Backend`] op
//! contract; [`build_backend`] turns a declarative [`SourceSpec`] into a live
//! backend. Out-of-process plugins speak gRPC [`SourceService`](pb).

mod conformance;
mod remote;
mod rt;
mod serve;

pub mod pb {
    tonic::include_proto!("vfs.source");
}

pub use conformance::{assert_conformance, write_fixture_tree};
pub use remote::RemoteBackend;
pub use serve::BackendSourceService;
pub use vfs_control::SourceSpec;
pub use vfs_protocol::{Backend, BackendHandle, DirEntry, Stat, KIND_DIR, KIND_FILE, OPEN_READ};

use std::sync::Arc;

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

/// Build a live backend from a declarative spec.
pub fn build_backend(spec: &SourceSpec) -> Result<Arc<dyn Backend>, BuildError> {
    match spec {
        SourceSpec::Disk { path } => Ok(Arc::new(vfs_director::DiskBackend::new(path))),
        SourceSpec::Zip { path } => {
            let be = vfs_zip::ZipBackend::open(std::path::Path::new(path))
                .map_err(|e| BuildError::Open(format!("{path}: {e:?}")))?;
            Ok(Arc::new(be))
        }
        SourceSpec::Http { .. } => Err(BuildError::Unsupported("http source (later milestone)")),
        SourceSpec::Remote { endpoint } => {
            let be = RemoteBackend::connect_blocking(endpoint)
                .map_err(BuildError::Open)?;
            Ok(Arc::new(be))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tonic::transport::Server;
    use vfs_director::DiskBackend;

    #[test]
    fn builds_disk_backend() {
        let dir = std::env::temp_dir().join(format!("vfs-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), b"hi").unwrap();
        let be = build_backend(&SourceSpec::Disk {
            path: dir.to_string_lossy().into_owned(),
        })
        .unwrap();
        let st = be.getattr("f.txt").unwrap().unwrap();
        assert_eq!(st.size, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_conformance() {
        let dir = std::env::temp_dir().join(format!("vfs-conf-{}", std::process::id()));
        write_fixture_tree(&dir);
        let be: Arc<dyn Backend> = Arc::new(DiskBackend::new(&dir));
        assert_conformance(be);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_backend_conformance() {
        let dir = std::env::temp_dir().join(format!("vfs-rconf-{}", std::process::id()));
        write_fixture_tree(&dir);
        let be: Arc<dyn Backend> = Arc::new(DiskBackend::new(&dir));
        let svc = BackendSourceService::new(be);
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

        let remote = RemoteBackend::connect(&format!("{addr}")).await.unwrap();
        let remote: Arc<dyn Backend> = Arc::new(remote);
        // Conformance is sync and uses block_on internally.
        tokio::task::spawn_blocking(move || {
            assert_conformance(remote);
        })
        .await
        .unwrap();

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn http_still_unsupported() {
        assert!(matches!(
            build_backend(&SourceSpec::Http { url: "http://x".into() }),
            Err(BuildError::Unsupported(_))
        ));
    }

    #[test]
    fn builds_zip_backend() {
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

        let be = build_backend(&SourceSpec::Zip {
            path: zip_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let st = be.getattr("f.txt").unwrap().unwrap();
        assert_eq!(st.size, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
