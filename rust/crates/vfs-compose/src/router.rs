//! Glob-based routing to providers (Clojure `aether.vfs.router`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_provider::{bad_fh, map_io_err, Capabilities, DirEntry, Handle, Provider, Stat, VPath};

use crate::glob;

/// One route: a glob pattern and the provider that owns matching paths.
pub struct Route {
    pub pattern: String,
    pub provider: Arc<dyn Provider>,
}

/// First matching route wins; otherwise `default`.
/// One open handle: the provider that answered, and the handle it returned.
type OpenEntry = (Arc<dyn Provider>, Handle);

/// Routes by glob pattern to a provider. Stage 1 keeps single-dispatch
/// `readdir` (only the matching route's — or default's — listing is
/// returned); a later stage unions across routes.
pub struct RouterProvider {
    default: Arc<dyn Provider>,
    routes: Vec<Route>,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, OpenEntry>>,
}

impl RouterProvider {
    pub fn new(default: Arc<dyn Provider>, routes: Vec<Route>) -> Self {
        Self {
            default,
            routes,
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
        }
    }

    pub fn provider_for(&self, path: &str) -> Arc<dyn Provider> {
        let with_slash = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        for r in &self.routes {
            if glob::matches(&r.pattern, &with_slash) {
                return Arc::clone(&r.provider);
            }
        }
        Arc::clone(&self.default)
    }
}

impl Provider for RouterProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities::weakest(
            std::iter::once(self.default.capabilities())
                .chain(self.routes.iter().map(|r| r.provider.capabilities())),
        )
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        let path = p.rel;
        self.provider_for(path).getattr(p)
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let path = p.rel;
        self.provider_for(path).readdir(p)
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let path = p.rel;
        let provider = self.provider_for(path);
        let (inner, size, is_dir) = provider.open(p, flags)?;
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens
            .lock()
            .map_err(|_| map_io_err())?
            .insert(h, (provider, inner));
        Ok((h, size, is_dir))
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let (provider, inner) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let (b, i) = g.get(&h).ok_or_else(bad_fh)?;
            (Arc::clone(b), *i)
        };
        provider.read_at(inner, offset, buf)
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        let (provider, inner) = {
            let mut g = self.opens.lock().map_err(|_| map_io_err())?;
            g.remove(&h).ok_or_else(bad_fh)?
        };
        provider.close(inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InlineProvider;
    use vfs_provider::OPEN_READ;

    fn tag(name: &'static str) -> Arc<dyn Provider> {
        Arc::new(InlineProvider::from_files([("tag", name.as_bytes())]))
    }

    fn tag_of(p: &dyn Provider) -> String {
        let (h, _, _) = p.open(VPath::at_default("tag"), OPEN_READ).unwrap();
        let mut buf = [0u8; 32];
        let n = p.read_at(h, 0, &mut buf).unwrap();
        p.close(h).unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    #[test]
    fn matched_path_routes_to_provider() {
        let r = RouterProvider::new(
            tag("default"),
            vec![Route {
                pattern: "/game/**".into(),
                provider: tag("game"),
            }],
        );
        assert_eq!(tag_of(r.provider_for("/game/a.dat").as_ref()), "game");
    }

    #[test]
    fn unmatched_path_routes_to_default() {
        let r = RouterProvider::new(
            tag("default"),
            vec![Route {
                pattern: "/game/**".into(),
                provider: tag("game"),
            }],
        );
        assert_eq!(
            tag_of(r.provider_for("/windows/system32").as_ref()),
            "default"
        );
    }

    #[test]
    fn first_matching_route_wins() {
        let r = RouterProvider::new(
            tag("default"),
            vec![
                Route {
                    pattern: "/game/*.exe".into(),
                    provider: tag("exe"),
                },
                Route {
                    pattern: "/game/**".into(),
                    provider: tag("game"),
                },
            ],
        );
        assert_eq!(tag_of(r.provider_for("/game/app.exe").as_ref()), "exe");
    }

    #[test]
    fn open_read_through_router_provider_trait() {
        // Routes select the provider; the path is passed through unchanged.
        let game = Arc::new(InlineProvider::from_files([(
            "game/a.dat",
            b"GAME".as_slice(),
        )]));
        let def = Arc::new(InlineProvider::from_files([(
            "other.txt",
            b"DEF".as_slice(),
        )]));
        let r = RouterProvider::new(
            def,
            vec![Route {
                pattern: "/game/**".into(),
                provider: game,
            }],
        );
        let (h, size, _) = r.open(VPath::at_default("game/a.dat"), OPEN_READ).unwrap();
        assert_eq!(size, 4);
        let mut buf = [0u8; 8];
        assert_eq!(r.read_at(h, 0, &mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], b"GAME");
        r.close(h).unwrap();

        let (h, size, _) = r.open(VPath::at_default("other.txt"), OPEN_READ).unwrap();
        assert_eq!(size, 3);
        assert_eq!(r.read_at(h, 0, &mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"DEF");
        r.close(h).unwrap();
    }
}
