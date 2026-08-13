//! Glob-based routing to backends (Clojure `aether.vfs.router`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_protocol::{
    bad_fh, map_io_err, Backend, BackendHandle, DirEntry, Stat,
};

use crate::glob;

/// One route: a glob pattern and the backend that owns matching paths.
pub struct Route {
    pub pattern: String,
    pub backend: Arc<dyn Backend>,
}

/// First matching route wins; otherwise `default`.
/// One open handle: the backend that answered, and the handle it returned.
type OpenEntry = (Arc<dyn Backend>, BackendHandle);

pub struct RouterBackend {
    default: Arc<dyn Backend>,
    routes: Vec<Route>,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, OpenEntry>>,
}

impl RouterBackend {
    pub fn new(default: Arc<dyn Backend>, routes: Vec<Route>) -> Self {
        Self {
            default,
            routes,
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
        }
    }

    pub fn provider_for(&self, path: &str) -> Arc<dyn Backend> {
        let with_slash = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        for r in &self.routes {
            if glob::matches(&r.pattern, &with_slash) {
                return Arc::clone(&r.backend);
            }
        }
        Arc::clone(&self.default)
    }
}

impl Backend for RouterBackend {
    fn getattr(&self, path: &str) -> Result<Option<Stat>, i32> {
        self.provider_for(path).getattr(path)
    }

    fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, i32> {
        self.provider_for(path).readdir(path)
    }

    fn open(&self, path: &str, flags: u32) -> Result<(BackendHandle, u64, bool), i32> {
        let backend = self.provider_for(path);
        let (inner, size, is_dir) = backend.open(path, flags)?;
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens
            .lock()
            .map_err(|_| map_io_err())?
            .insert(h, (backend, inner));
        Ok((h, size, is_dir))
    }

    fn read(&self, bh: BackendHandle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let (backend, inner) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let (b, i) = g.get(&bh).ok_or_else(bad_fh)?;
            (Arc::clone(b), *i)
        };
        backend.read(inner, offset, buf)
    }

    fn release(&self, bh: BackendHandle) -> Result<(), i32> {
        let (backend, inner) = {
            let mut g = self.opens.lock().map_err(|_| map_io_err())?;
            g.remove(&bh).ok_or_else(bad_fh)?
        };
        backend.release(inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InlineBackend;
    use vfs_protocol::OPEN_READ;

    fn tag(name: &'static str) -> Arc<dyn Backend> {
        Arc::new(InlineBackend::from_files([("tag", name.as_bytes())]))
    }

    fn tag_of(be: &dyn Backend) -> String {
        let (h, _, _) = be.open("tag", OPEN_READ).unwrap();
        let mut buf = [0u8; 32];
        let n = be.read(h, 0, &mut buf).unwrap();
        be.release(h).unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    #[test]
    fn matched_path_routes_to_provider() {
        let r = RouterBackend::new(
            tag("default"),
            vec![Route {
                pattern: "/game/**".into(),
                backend: tag("game"),
            }],
        );
        assert_eq!(tag_of(r.provider_for("/game/a.dat").as_ref()), "game");
    }

    #[test]
    fn unmatched_path_routes_to_default() {
        let r = RouterBackend::new(
            tag("default"),
            vec![Route {
                pattern: "/game/**".into(),
                backend: tag("game"),
            }],
        );
        assert_eq!(
            tag_of(r.provider_for("/windows/system32").as_ref()),
            "default"
        );
    }

    #[test]
    fn first_matching_route_wins() {
        let r = RouterBackend::new(
            tag("default"),
            vec![
                Route {
                    pattern: "/game/*.exe".into(),
                    backend: tag("exe"),
                },
                Route {
                    pattern: "/game/**".into(),
                    backend: tag("game"),
                },
            ],
        );
        assert_eq!(tag_of(r.provider_for("/game/app.exe").as_ref()), "exe");
    }

    #[test]
    fn open_read_through_router_backend_trait() {
        // Routes select the backend; the path is passed through unchanged.
        let game = Arc::new(InlineBackend::from_files([("game/a.dat", b"GAME".as_slice())]));
        let def = Arc::new(InlineBackend::from_files([("other.txt", b"DEF".as_slice())]));
        let r = RouterBackend::new(
            def,
            vec![Route {
                pattern: "/game/**".into(),
                backend: game,
            }],
        );
        let (h, size, _) = r.open("game/a.dat", OPEN_READ).unwrap();
        assert_eq!(size, 4);
        let mut buf = [0u8; 8];
        assert_eq!(r.read(h, 0, &mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], b"GAME");
        r.release(h).unwrap();

        let (h, size, _) = r.open("other.txt", OPEN_READ).unwrap();
        assert_eq!(size, 3);
        assert_eq!(r.read(h, 0, &mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"DEF");
        r.release(h).unwrap();
    }
}
