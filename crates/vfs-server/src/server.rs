//! The authoritative Server.

use vfs_core::{build, BuildError, Layer, VfsTree};
use vfs_ipc::ring::IpcError;
use vfs_ipc::{Notifier, RingServer};

use crate::handler::dispatch;

/// The authoritative server: owns the merged tree, answers ring requests, and
/// publishes the shared-memory snapshot.
pub struct Server {
    tree: VfsTree,
}

impl Server {
    pub fn new(tree: VfsTree) -> Self {
        Server { tree }
    }

    pub fn from_layers(layers: Vec<Layer>) -> Result<Self, BuildError> {
        Ok(Server { tree: build(layers)? })
    }

    pub fn tree(&self) -> &VfsTree {
        &self.tree
    }

    /// Decode + answer one (opcode, payload).
    pub fn handle(&self, opcode: u32, payload: &[u8]) -> (i32, Vec<u8>) {
        dispatch(&self.tree, opcode, payload)
    }

    /// Serve one request off a ring (Ok(true) if one was handled).
    pub fn serve_one<N: Notifier>(&self, ring: &RingServer<'_, N>) -> Result<bool, IpcError> {
        ring.serve_one(|req| self.handle(req.opcode, &req.payload))
    }

    /// Publish the authoritative tree as a vfs-shared snapshot image.
    pub fn snapshot(&self) -> Vec<u8> {
        vfs_shared::bridge::flatten(&self.tree)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_core::{EntryKind, InputEntry, LayerId};
    use vfs_ipc::layout::OP_GETATTR;
    use vfs_shared::{SnapResolution, SnapshotReader};

    use crate::proto::{decode_getattr_resp, encode_path_req};

    fn server() -> Server {
        Server::from_layers(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/a.esp".into(),
                kind: EntryKind::File,
                source: "s/a".into(),
                size: 10,
                mtime: 1,
            }],
        }])
        .unwrap()
    }

    #[test]
    fn handle_delegates_to_dispatch() {
        let s = server();
        let (st, p) = s.handle(OP_GETATTR, &encode_path_req("data/a.esp"));
        assert_eq!(st, 0);
        assert!(decode_getattr_resp(&p).unwrap().found);
    }

    #[test]
    fn snapshot_matches_tree() {
        let s = server();
        let img = s.snapshot();
        let r = SnapshotReader::open(&img).unwrap();
        // Folded keys: "data" / "a.esp".
        assert!(matches!(r.resolve(&["data", "a.esp"]), SnapResolution::File { .. }));
        assert_eq!(r.getattr(&["data", "a.esp"]).unwrap().size, 10);
    }
}
