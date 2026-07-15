//! The authoritative Server.

use vfs_core::{build, BuildError, Layer, VfsTree};
use vfs_ipc::ring::IpcError;
use vfs_ipc::{Notifier, RingServer};

use crate::arena::DataArena;
use crate::handler::{dispatch, dispatch_full, dispatch_with_table};
use crate::open_table::OpenTable;
use crate::remote::RemoteMemWriter;

/// **B2:** default ring payload capacity (1 MiB).
pub const DEFAULT_PAYLOAD_CAP: u32 = 1_048_576;
/// **B3:** default number of server worker threads.
pub const DEFAULT_WORKER_COUNT: usize = 4;

/// The authoritative server: owns the merged tree, open-file table, and answers
/// ring requests (and can still publish a snapshot for debug).
pub struct Server {
    tree: VfsTree,
    table: OpenTable,
    payload_cap: u32,
}

impl Server {
    pub fn new(tree: VfsTree) -> Self {
        Self::with_payload_cap(tree, DEFAULT_PAYLOAD_CAP)
    }

    pub fn with_payload_cap(tree: VfsTree, payload_cap: u32) -> Self {
        Server {
            tree,
            table: OpenTable::new(),
            payload_cap,
        }
    }

    pub fn from_layers(layers: Vec<Layer>) -> Result<Self, BuildError> {
        Ok(Server::new(build(layers)?))
    }

    pub fn from_layers_with_cap(layers: Vec<Layer>, payload_cap: u32) -> Result<Self, BuildError> {
        Ok(Server::with_payload_cap(build(layers)?, payload_cap))
    }

    pub fn tree(&self) -> &VfsTree {
        &self.tree
    }

    pub fn payload_cap(&self) -> u32 {
        self.payload_cap
    }

    pub fn table(&self) -> &OpenTable {
        &self.table
    }

    /// Decode + answer one (opcode, payload) — includes OPEN/READ/CLOSE.
    pub fn handle(&self, opcode: u32, payload: &[u8]) -> (i32, Vec<u8>) {
        dispatch_with_table(
            &self.tree,
            &self.table,
            opcode,
            payload,
            self.payload_cap,
        )
    }

    pub fn handle_meta(&self, opcode: u32, payload: &[u8]) -> (i32, Vec<u8>) {
        dispatch(&self.tree, opcode, payload)
    }

    /// Serve one request off a ring (Ok(true) if one was handled).
    pub fn serve_one<N: Notifier>(&self, ring: &RingServer<'_, N>) -> Result<bool, IpcError> {
        ring.serve_one(|req| self.handle(req.opcode, &req.payload))
    }

    /// **B1:** serve with bulk arena (slot-keyed banks).
    pub fn serve_one_arena<N: Notifier>(
        &self,
        ring: &RingServer<'_, N>,
        arena: &DataArena<'_>,
    ) -> Result<bool, IpcError> {
        self.serve_one_arena_remote(ring, arena, None)
    }

    /// **B1 + phase 2:** bulk arena + optional remote (WPM) writer.
    pub fn serve_one_arena_remote<N: Notifier>(
        &self,
        ring: &RingServer<'_, N>,
        arena: &DataArena<'_>,
        remote: Option<&dyn RemoteMemWriter>,
    ) -> Result<bool, IpcError> {
        ring.serve_one(|req| {
            dispatch_full(
                &self.tree,
                &self.table,
                req.opcode,
                &req.payload,
                req.flags,
                self.payload_cap,
                Some((arena, req.slot)),
                remote,
            )
        })
    }

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
        assert!(matches!(
            r.resolve(&["data", "a.esp"]),
            SnapResolution::File { .. }
        ));
        assert_eq!(r.getattr(&["data", "a.esp"]).unwrap().size, 10);
    }
}
