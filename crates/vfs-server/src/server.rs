//! The authoritative Server.

use std::sync::Arc;

use vfs_core::{build, BuildError, Layer, VfsTree};
use vfs_director::Director;
use vfs_ipc::ring::IpcError;
use vfs_ipc::{Notifier, RingServer};

use crate::arena::DataArena;
use crate::director_dispatch::dispatch_director;
use crate::handler::{dispatch, dispatch_full, dispatch_with_table};
use crate::open_table::OpenTable;

/// **B2:** default ring payload capacity (1 MiB).
pub const DEFAULT_PAYLOAD_CAP: u32 = 1_048_576;
/// **B3:** default number of server worker threads.
pub const DEFAULT_WORKER_COUNT: usize = 4;

/// Content authority for ring requests.
enum Authority {
    /// Legacy: vfs-core tree + open table (zip-window sources).
    Tree {
        tree: VfsTree,
        table: OpenTable,
    },
    /// Userspace FUSE kernel with mounted backends (preferred).
    Director(Arc<Director>),
}

/// The authoritative server: answers ring requests from a tree or a director.
pub struct Server {
    authority: Authority,
    payload_cap: u32,
}

impl Server {
    pub fn new(tree: VfsTree) -> Self {
        Self::with_payload_cap(tree, DEFAULT_PAYLOAD_CAP)
    }

    pub fn with_payload_cap(tree: VfsTree, payload_cap: u32) -> Self {
        Server {
            authority: Authority::Tree {
                tree,
                table: OpenTable::new(),
            },
            payload_cap,
        }
    }

    /// Preferred: ring content path goes through the userspace FUSE director.
    pub fn from_director(director: Arc<Director>, payload_cap: u32) -> Self {
        Server {
            authority: Authority::Director(director),
            payload_cap,
        }
    }

    pub fn from_layers(layers: Vec<Layer>) -> Result<Self, BuildError> {
        Ok(Server::new(build(layers)?))
    }

    pub fn from_layers_with_cap(layers: Vec<Layer>, payload_cap: u32) -> Result<Self, BuildError> {
        Ok(Server::with_payload_cap(build(layers)?, payload_cap))
    }

    pub fn tree(&self) -> Option<&VfsTree> {
        match &self.authority {
            Authority::Tree { tree, .. } => Some(tree),
            Authority::Director(_) => None,
        }
    }

    pub fn director(&self) -> Option<&Arc<Director>> {
        match &self.authority {
            Authority::Director(d) => Some(d),
            Authority::Tree { .. } => None,
        }
    }

    pub fn payload_cap(&self) -> u32 {
        self.payload_cap
    }

    pub fn table(&self) -> Option<&OpenTable> {
        match &self.authority {
            Authority::Tree { table, .. } => Some(table),
            Authority::Director(_) => None,
        }
    }

    /// Decode + answer one (opcode, payload) — includes OPEN/READ/CLOSE.
    pub fn handle(&self, opcode: u32, payload: &[u8]) -> (i32, Vec<u8>) {
        match &self.authority {
            Authority::Tree { tree, table } => {
                dispatch_with_table(tree, table, opcode, payload, self.payload_cap)
            }
            Authority::Director(d) => {
                dispatch_director(d, opcode, payload, 0, self.payload_cap, None)
            }
        }
    }

    pub fn handle_meta(&self, opcode: u32, payload: &[u8]) -> (i32, Vec<u8>) {
        match &self.authority {
            Authority::Tree { tree, .. } => dispatch(tree, opcode, payload),
            Authority::Director(d) => {
                dispatch_director(d, opcode, payload, 0, self.payload_cap, None)
            }
        }
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
        ring.serve_one(|req| match &self.authority {
            Authority::Tree { tree, table } => dispatch_full(
                tree,
                table,
                req.opcode,
                &req.payload,
                req.flags,
                self.payload_cap,
                Some((arena, req.slot)),
            ),
            Authority::Director(d) => dispatch_director(
                d,
                req.opcode,
                &req.payload,
                req.flags,
                self.payload_cap,
                Some((arena, req.slot)),
            ),
        })
    }

    pub fn snapshot(&self) -> Option<Vec<u8>> {
        match &self.authority {
            Authority::Tree { tree, .. } => Some(vfs_shared::bridge::flatten(tree)),
            Authority::Director(_) => None,
        }
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
        let img = s.snapshot().unwrap();
        let r = SnapshotReader::open(&img).unwrap();
        assert!(matches!(
            r.resolve(&["data", "a.esp"]),
            SnapResolution::File { .. }
        ));
        assert_eq!(r.getattr(&["data", "a.esp"]).unwrap().size, 10);
    }
}
