//! Reference out-of-process source plugin: serves a disk directory over Source gRPC.
//!
//! ```text
//! vfs-source-plugin --root C:\data --bind 127.0.0.1:0
//! ```
//! Prints `endpoint=host:port` on stdout for clients / tests.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::net::TcpListener;
use tonic::transport::Server;
use vfs_director::DiskBackend;
use vfs_source::pb::source_server::SourceServer;
use vfs_source::BackendSourceService;

#[derive(Parser, Debug)]
#[command(name = "vfs-source-plugin")]
struct Args {
    /// Host directory to serve.
    #[arg(long)]
    root: PathBuf,
    /// Bind address (`host:port`, port 0 = ephemeral).
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let addr: SocketAddr = args.bind.parse()?;
    let backend = Arc::new(DiskBackend::new(&args.root));
    let svc = BackendSourceService::new(backend);
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    println!("endpoint={}:{}", local.ip(), local.port());
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    Server::builder()
        .add_service(SourceServer::new(svc))
        .serve_with_incoming(incoming)
        .await?;
    Ok(())
}
