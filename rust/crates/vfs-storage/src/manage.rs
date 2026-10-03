//! Layer management: list, export, import and delete (spec §7), and
//! [`Storage::stats`].

use std::fs;
use std::io::Read;
use std::io::Write;
use std::path::{Component, Path};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use vfs_block_store::{ClassWriteStats, CompactOptions, Usage, WriteClass, WriteStats};
use vfs_provider::{
    Provider, SetAttr, VPath, KIND_DIR, KIND_FILE, OPEN_CREATE, OPEN_READ, OPEN_TRUNC, OPEN_WRITE,
};

use crate::cached::{lock, CacheStats};
use crate::ids::{classify_store_id, layer_file_id, Guid, StoreIdKind};
use crate::layer::LayerProvider;
use crate::storage::{Storage, StorageError};

/// The overlay's whiteout prefix: `OverlayProvider` hides `<name>` by writing
/// `.wh.<name>` into its upper (`vfs-compose/src/overlay.rs`, which does not
/// export it as a constant). Duplicated here; keep the two in step.
const WHITEOUT_PREFIX: &str = ".wh.";
/// The overlay's copy-up staging prefix, `.cu.<n>.<name>`
/// (`vfs-compose/src/overlay.rs`): a half-finished copy a crash left behind,
/// which the overlay never serves either.
const COPY_UP_PREFIX: &str = ".cu.";

/// Bytes moved per read or write when exporting or importing.
const CHUNK: usize = 1 << 20;

/// One layer, as [`Storage::layers`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerInfo {
    pub name: String,
    /// Files (not directories), overlay markers included.
    pub files: u64,
    /// The sum of the files' lengths as last committed to the catalog.
    pub logical_bytes: u64,
}

/// [`Storage::cache_stats`] plus the block store's space and the layer count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StorageStats {
    pub cache: CacheStats,
    /// Bytes of the store's pack files on disk (active and sealed packs).
    pub pack_bytes: u64,
    /// Bytes of those packs still referenced; `pack_bytes - live_bytes` is
    /// what compaction can reclaim.
    pub live_bytes: u64,
    pub layer_count: u64,
}

/// Stored against logical bytes, per kind of store file:
/// [`Storage::space_usage`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpaceUsage {
    /// Every pull-through cache file.
    pub cache: Usage,
    /// Every layer's files, by layer name.
    pub layers: std::collections::BTreeMap<String, Usage>,
    /// Store files that belong to no catalog row (layer files waiting for
    /// deletion at the next durable point, orphans).
    pub unlisted: Usage,
}

/// Names the overlay reserves in its upper; `export_layer` skips them.
fn is_overlay_marker(name: &str) -> bool {
    name.starts_with(WHITEOUT_PREFIX) || name.starts_with(COPY_UP_PREFIX)
}

fn at(rel: &str) -> VPath<'_> {
    VPath::at_default(rel)
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// A provider status from a layer call, as a `StorageError`.
fn layer_err(what: &str, rel: &str, status: i32) -> StorageError {
    StorageError::Io(std::io::Error::other(format!(
        "layer {what} {rel:?}: provider status {status}"
    )))
}

/// One path component, as exported to or imported from the host: exactly
/// one plain component (`Path::components` yields a single `Normal`), with no
/// separator (the layer treats `\` as one) and no `:`. So it can neither step
/// outside its directory (`..`, a root, a Windows drive or `C:rel` prefix,
/// which `Path::join` would let replace the base) nor split into two names.
fn host_component(name: &str) -> Result<&str, StorageError> {
    let mut c = Path::new(name).components();
    let single = matches!(
        (c.next(), c.next()),
        (Some(Component::Normal(n)), None) if n == std::ffi::OsStr::new(name)
    );
    if !single || name.contains(['/', '\\', ':']) {
        return Err(StorageError::BadRequest(format!(
            "layer entry name {name:?} is not a valid file name"
        )));
    }
    Ok(name)
}

impl Storage {
    /// Every layer with its file count and logical bytes, sorted by name.
    pub fn layers(&self) -> Result<Vec<LayerInfo>, StorageError> {
        let totals = self.catalog.layer_file_totals()?;
        let mut out: Vec<LayerInfo> = self
            .catalog
            .layer_names()?
            .into_iter()
            .map(|(name, id)| {
                let (files, logical_bytes) = totals.get(&id).copied().unwrap_or((0, 0));
                LayerInfo {
                    name,
                    files,
                    logical_bytes,
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Writes layer `name` into `dir` as plain files and directories, and
    /// returns the number of files written. Overlay markers (`.wh.` whiteouts,
    /// `.cu.` copy-up leftovers) are skipped. File modification times are
    /// carried over. `dir` is created if missing; one that holds anything is
    /// refused with `Exists`.
    ///
    /// Reads go through the layer's provider (the live one, if a session has
    /// it open), so a file still open for writing is exported as its current
    /// bytes.
    pub fn export_layer(self: &Arc<Self>, name: &str, dir: &Path) -> Result<u64, StorageError> {
        let p = self.layer_provider(name, false)?;
        match fs::read_dir(dir) {
            Ok(mut it) => {
                if it.next().is_some() {
                    return Err(StorageError::Exists(dir.display().to_string()));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir_all(dir)?,
            Err(e) => return Err(e.into()),
        }
        let mut files = 0;
        export_dir(&p, "", dir, &mut files)?;
        Ok(files)
    }

    /// Creates layer `name` from the tree under `dir` and returns the number of
    /// files copied in. Refused with `LayerExists` if the layer exists. The
    /// layer is durable when this returns; if the copy fails partway, the
    /// partial layer is deleted.
    ///
    /// Symbolic links to files are followed; links to directories are skipped
    /// with a warning (they could loop), as is anything neither file nor
    /// directory.
    pub fn import_layer(self: &Arc<Self>, dir: &Path, name: &str) -> Result<u64, StorageError> {
        if name.is_empty() {
            return Err(StorageError::BadRequest("empty layer name".into()));
        }
        if !fs::metadata(dir)?.is_dir() {
            return Err(StorageError::BadRequest(format!(
                "{} is not a directory",
                dir.display()
            )));
        }
        self.create_layer_durably(name)?;
        let p = self.layer_provider(name, false)?;
        let mut files = 0;
        let copied = import_dir(&p, dir, "", &mut files)
            .and_then(|()| p.durable_point().map_err(|st| layer_err("flush", name, st)));
        drop(p);
        if let Err(e) = copied {
            if let Err(d) = self.delete_layer(name) {
                tracing::warn!(layer = name, error = %d, "failed import: partial layer not deleted");
            }
            return Err(e);
        }
        Ok(files)
    }

    /// Deletes layer `name` and its data. Refused with `LayerInUse` while a
    /// provider for it exists (a session uses it, or its last provider is
    /// still finishing its final commit), and `NoSuchLayer` if there is none.
    ///
    /// Spec §6 order: the catalog rows go first and durably, then the store
    /// files, then a compaction to reclaim their space. A crash in between
    /// leaves store orphans, which reconciliation at the next open deletes.
    pub fn delete_layer(&self, name: &str) -> Result<(), StorageError> {
        let guids = {
            // Held so no provider for `name` can appear while its rows go.
            let layers = lock(&self.layers);
            if layers.contains_key(name) {
                return Err(StorageError::LayerInUse(name.to_owned()));
            }
            let id = self
                .catalog
                .layer_id(name)?
                .ok_or_else(|| StorageError::NoSuchLayer(name.to_owned()))?;
            self.catalog.drop_layer(id)?
        };
        // Every durable catalog row must reference durable store data, and the
        // commit makes other layers' pending rows durable too.
        self.flush_durably()?;
        let mut deleted = false;
        for g in guids {
            let id = layer_file_id(&g);
            self.ram.invalidate_file(&id);
            match self.store.delete(&id) {
                Ok(()) => deleted = true,
                Err(vfs_block_store::Error::NotFound) => {}
                Err(e) => tracing::warn!(
                    layer = name, error = %e,
                    "layer file delete failed; reconciliation will retry"
                ),
            }
        }
        if deleted {
            if let Err(e) = self.store.compact(CompactOptions::default()) {
                tracing::warn!(layer = name, error = %e, "compaction after layer delete failed");
            }
        }
        Ok(())
    }

    /// Stored (compressed, deduplicated within a kind) against logical bytes
    /// of the cache and of each layer. Reads every block row the store's
    /// manifests reference: about a second per few million blocks.
    pub fn space_usage(&self) -> Result<SpaceUsage, StorageError> {
        let names: std::collections::HashMap<u64, String> = self
            .catalog
            .layer_names()?
            .into_iter()
            .map(|(n, id)| (id, n))
            .collect();
        let owner: std::collections::HashMap<Guid, String> = self
            .catalog
            .all_layer_guids()?
            .into_iter()
            .filter_map(|(layer, _, g)| names.get(&layer).map(|n| (g, n.clone())))
            .collect();
        #[derive(Clone, PartialEq, Eq, Hash)]
        enum Kind {
            Cache,
            Layer(String),
            Unlisted,
        }
        let by = self.store.usage_by(|id| {
            Some(match classify_store_id(id) {
                StoreIdKind::Cache(_) => Kind::Cache,
                StoreIdKind::Layer(g) => owner.get(&g).cloned().map_or(Kind::Unlisted, Kind::Layer),
                StoreIdKind::Foreign => Kind::Unlisted,
            })
        })?;
        let mut out = SpaceUsage::default();
        for (k, u) in by {
            match k {
                Kind::Cache => out.cache = u,
                Kind::Layer(n) => {
                    out.layers.insert(n, u);
                }
                Kind::Unlisted => out.unlisted = u,
            }
        }
        Ok(out)
    }

    /// What writes stored since open, per write class (see
    /// [`vfs_block_store::BlockStore::write_stats`]).
    pub fn write_stats(&self) -> WriteStats {
        self.store.write_stats()
    }

    /// Both classes' [`Storage::write_stats`] together.
    pub fn written(&self) -> ClassWriteStats {
        let s = self.store.write_stats();
        s.foreground.plus(&s.bulk)
    }

    /// What compresses `class` writes, for logs: `zstd:6`, `GPU opt16p1`, ...
    pub fn compression(&self, class: WriteClass) -> String {
        self.store.compression(class)
    }

    /// Cache counters, the store's pack space, and the number of layers.
    /// A failure to read the store's or catalog's figures is logged and
    /// reported as zero.
    pub fn stats(&self) -> StorageStats {
        let (pack_bytes, live_bytes) = match self.store.stats() {
            Ok(st) => st.packs.iter().fold((0u64, 0u64), |(f, l), p| {
                (f + p.file_bytes, l + p.live_bytes)
            }),
            Err(e) => {
                tracing::warn!(error = %e, "block store stats failed");
                (0, 0)
            }
        };
        let layer_count = match self.catalog.layer_names() {
            Ok(v) => v.len() as u64,
            Err(e) => {
                tracing::warn!(error = %e, "catalog layer list failed");
                0
            }
        };
        StorageStats {
            cache: self.cache_stats(),
            pack_bytes,
            live_bytes,
            layer_count,
        }
    }
}

fn export_dir(
    p: &LayerProvider,
    rel: &str,
    out: &Path,
    files: &mut u64,
) -> Result<(), StorageError> {
    let ents = p
        .readdir(at(rel))
        .map_err(|st| layer_err("readdir", rel, st))?;
    for e in ents {
        if is_overlay_marker(&e.name) {
            continue;
        }
        let child = join(rel, &e.name);
        let dest = out.join(host_component(&e.name)?);
        match e.stat.kind {
            KIND_DIR => {
                fs::create_dir(&dest)?;
                export_dir(p, &child, &dest, files)?;
            }
            KIND_FILE => {
                export_file(p, &child, &dest, e.stat.mtime)?;
                *files += 1;
            }
            _ => {}
        }
    }
    Ok(())
}

fn export_file(p: &LayerProvider, rel: &str, dest: &Path, mtime: i64) -> Result<(), StorageError> {
    let (h, size, _) = p
        .open(at(rel), OPEN_READ)
        .map_err(|st| layer_err("open", rel, st))?;
    let copied: Result<(), StorageError> = (|| {
        let mut f = fs::File::create_new(dest)?;
        let mut buf = vec![0u8; CHUNK.min(size as usize)];
        let mut off = 0u64;
        while off < size {
            let want = buf.len().min((size - off) as usize);
            let n = p
                .read_at(h, off, &mut buf[..want])
                .map_err(|st| layer_err("read", rel, st))?;
            if n == 0 {
                return Err(layer_err(
                    "read (short file)",
                    rel,
                    vfs_provider::ST_IO_ERROR,
                ));
            }
            f.write_all(&buf[..n])?;
            off += n as u64;
        }
        if mtime > 0 {
            f.set_modified(UNIX_EPOCH + Duration::from_secs(mtime as u64))?;
        }
        Ok(())
    })();
    let closed = p.close(h).map_err(|st| layer_err("close", rel, st));
    copied?;
    closed
}

fn import_dir(
    p: &LayerProvider,
    src: &Path,
    rel: &str,
    files: &mut u64,
) -> Result<(), StorageError> {
    let mut ents = Vec::new();
    for e in fs::read_dir(src)? {
        let e = e?;
        let name = e.file_name().into_string().map_err(|n| {
            StorageError::BadRequest(format!("{n:?} in {}: not UTF-8", src.display()))
        })?;
        host_component(&name)?;
        ents.push((name, e));
    }
    ents.sort_by(|a, b| a.0.cmp(&b.0));
    // The layer is case-insensitive: two names that fold alike would land on
    // one entry, the second silently replacing the first.
    let mut folded = std::collections::HashMap::new();
    for (name, _) in &ents {
        if let Some(other) = folded.insert(vfs_core::fold(name), name) {
            return Err(StorageError::BadRequest(format!(
                "{other:?} and {name:?} in {} differ only in case; a layer is \
                 case-insensitive and cannot hold both",
                src.display()
            )));
        }
    }
    for (name, e) in ents {
        let child = join(rel, &name);
        let path = e.path();
        let ft = e.file_type()?;
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(err) if ft.is_symlink() => {
                tracing::warn!(path = %path.display(), error = %err, "import: broken symbolic link; skipped");
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        if meta.is_dir() && !ft.is_symlink() {
            p.mkdir(at(&child))
                .map_err(|st| layer_err("mkdir", &child, st))?;
            import_dir(p, &path, &child, files)?;
        } else if meta.is_file() {
            import_file(p, &path, &child, &meta)?;
            *files += 1;
        } else {
            tracing::warn!(path = %path.display(), "import: not a file or directory; skipped");
        }
    }
    Ok(())
}

fn import_file(
    p: &LayerProvider,
    src: &Path,
    rel: &str,
    meta: &fs::Metadata,
) -> Result<(), StorageError> {
    #[cfg(test)]
    if crate::cached::lock(&p.storage().fail_import_at).as_deref() == Some(rel) {
        return Err(StorageError::Io(std::io::Error::other(
            "injected import failure",
        )));
    }
    let mut f = fs::File::open(src)?;
    let (h, _, _) = p
        .open(at(rel), OPEN_WRITE | OPEN_CREATE | OPEN_TRUNC)
        .map_err(|st| layer_err("create", rel, st))?;
    let copied: Result<(), StorageError> = (|| {
        let mut buf = vec![0u8; CHUNK];
        let mut off = 0u64;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            let w = p
                .write_at(h, off, &buf[..n])
                .map_err(|st| layer_err("write", rel, st))?;
            off += w as u64;
        }
        // Set while the handle is open, so the commit at close records it.
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        if let Some(mtime) = mtime {
            p.set_attr(
                at(rel),
                SetAttr {
                    mtime: Some(mtime),
                    size: None,
                },
            )
            .map_err(|st| layer_err("set mtime", rel, st))?;
        }
        Ok(())
    })();
    // The close of a handle that wrote is the file's durable point.
    let closed = p.close(h).map_err(|st| layer_err("close", rel, st));
    copied?;
    closed
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{mpsc, Arc};

    use vfs_provider::{
        Provider, SetAttr, VPath, KIND_DIR, KIND_FILE, OPEN_CREATE, OPEN_READ, OPEN_WRITE,
    };

    use crate::config::StorageConfig;
    use crate::ids::layer_file_id;
    use crate::storage::{Storage, StorageError};

    use super::LayerInfo;

    const BS: u64 = 4096;

    fn cfg() -> StorageConfig {
        let mut c = StorageConfig::default();
        c.store.block_size = BS as u32;
        c
    }

    fn temp_storage() -> (Arc<Storage>, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path().join("store"), cfg()).unwrap();
        (s, d)
    }

    fn at(p: &str) -> VPath<'_> {
        VPath::at_default(p)
    }

    fn write_file(p: &Arc<dyn Provider>, rel: &str, body: &[u8]) {
        let (h, _, _) = p.open(at(rel), OPEN_WRITE | OPEN_CREATE).unwrap();
        assert_eq!(p.write_at(h, 0, body).unwrap(), body.len());
        p.close(h).unwrap();
    }

    fn read_file(p: &Arc<dyn Provider>, rel: &str) -> Vec<u8> {
        let (h, size, _) = p.open(at(rel), OPEN_READ).unwrap();
        let mut out = vec![0u8; size as usize];
        let mut done = 0;
        while done < out.len() {
            let n = p.read_at(h, done as u64, &mut out[done..]).unwrap();
            if n == 0 {
                break;
            }
            done += n;
        }
        out.truncate(done);
        p.close(h).unwrap();
        out
    }

    /// The whole tree under `dir`: `(path, kind, bytes, mtime)` in order, with
    /// the original spelling of every name.
    fn tree(p: &Arc<dyn Provider>, dir: &str) -> Vec<(String, u8, Vec<u8>, i64)> {
        let mut out = Vec::new();
        let mut ents = p.readdir(at(dir)).unwrap();
        ents.sort_by(|a, b| a.name.cmp(&b.name));
        for e in ents {
            let rel = if dir.is_empty() {
                e.name.clone()
            } else {
                format!("{dir}/{}", e.name)
            };
            if e.stat.kind == KIND_DIR {
                out.push((rel.clone(), KIND_DIR, Vec::new(), 0));
                out.extend(tree(p, &rel));
            } else {
                out.push((rel.clone(), KIND_FILE, read_file(p, &rel), e.stat.mtime));
            }
        }
        out
    }

    fn disk_names(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().into_string().unwrap();
            if e.file_type().unwrap().is_dir() {
                out.push(format!("{name}/"));
                for n in disk_names(&e.path()) {
                    out.push(format!("{name}/{n}"));
                }
            } else {
                out.push(name);
            }
        }
        out.sort();
        out
    }

    #[test]
    fn export_import_round_trip() {
        let (s, d) = temp_storage();
        let upper = s.layer("src").unwrap();
        let big: Vec<u8> = (0..(3 * BS + 7)).map(|i| (i % 253) as u8).collect();
        upper.mkdir(at("Empty")).unwrap();
        write_file(&upper, "a.txt", b"alpha");
        write_file(&upper, "Sub/Deep/b.bin", &big);
        upper
            .set_attr(
                at("a.txt"),
                SetAttr {
                    mtime: Some(1_000_000),
                    ..Default::default()
                },
            )
            .unwrap();
        // A real whiteout, made by the overlay removing a base file.
        let base: Arc<dyn Provider> = Arc::new(vfs_provider::conformance::MemFixture::new());
        let ov = vfs_compose::OverlayProvider::from_arcs(base, Arc::clone(&upper)).unwrap();
        let victim = vfs_provider::FIXTURE_FILES[1].0; // sub/b.txt
        ov.remove(at(victim)).unwrap();
        drop(ov);
        let marker = match victim.rsplit_once('/') {
            Some((dir, name)) => format!("{dir}/.wh.{name}"),
            None => format!(".wh.{victim}"),
        };
        assert!(upper.getattr(at(&marker)).unwrap().is_some());
        // A copy-up temp file a crash left behind.
        write_file(&upper, "Sub/.cu.3.b.bin", b"half");
        let want: Vec<_> = tree(&upper, "")
            .into_iter()
            .filter(|(p, ..)| !p.contains("/.wh.") && !p.starts_with(".wh."))
            .filter(|(p, ..)| !p.contains(".cu."))
            .collect();

        let out = d.path().join("out");
        assert_eq!(s.export_layer("src", &out).unwrap(), 2);
        let names = disk_names(&out);
        assert!(
            names
                .iter()
                .all(|n| !n.contains(".wh.") && !n.contains(".cu.")),
            "{names:?}"
        );
        for n in ["Empty/", "Sub/", "Sub/Deep/", "Sub/Deep/b.bin", "a.txt"] {
            assert!(names.contains(&n.to_string()), "{n} in {names:?}");
        }
        assert_eq!(std::fs::read(out.join("Sub/Deep/b.bin")).unwrap(), big);
        let mtime = std::fs::metadata(out.join("a.txt"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(mtime, 1_000_000);

        // A non-empty target is refused; a missing layer is not found.
        assert!(matches!(
            s.export_layer("src", &out),
            Err(StorageError::Exists(_))
        ));
        assert!(matches!(
            s.export_layer("nope", &d.path().join("x")),
            Err(StorageError::NoSuchLayer(_))
        ));
        assert!(!d.path().join("x").exists());

        assert_eq!(s.import_layer(&out, "dst").unwrap(), 2);
        let dst = s.layer("dst").unwrap();
        let got = tree(&dst, "");
        // Directory mtimes are not carried; compare files' fully.
        assert_eq!(got, want);
        assert!(matches!(
            s.import_layer(&out, "dst"),
            Err(StorageError::LayerExists(_))
        ));
    }

    #[test]
    fn import_is_durable_and_a_failed_import_leaves_no_layer() {
        let (s, d) = temp_storage();
        let src = d.path().join("in");
        std::fs::create_dir_all(src.join("Saves")).unwrap();
        std::fs::write(src.join("Saves/one.ess"), b"one").unwrap();
        assert_eq!(s.import_layer(&src, "imp").unwrap(), 1);
        assert!(matches!(
            s.import_layer(&d.path().join("missing"), "other"),
            Err(StorageError::Io(_))
        ));
        assert!(s.layers().unwrap().iter().all(|l| l.name != "other"));

        // A failure after the layer exists and holds files: the partial layer
        // and its store data are deleted, and the name is free again.
        std::fs::write(src.join("a.txt"), b"first").unwrap();
        *s.fail_import_at.lock().unwrap() = Some("Saves/one.ess".into());
        let before: Vec<_> = s.store.file_ids().unwrap();
        assert!(matches!(
            s.import_layer(&src, "partial"),
            Err(StorageError::Io(_))
        ));
        assert!(s.layers().unwrap().iter().all(|l| l.name != "partial"));
        assert_eq!(s.store.file_ids().unwrap(), before, "no store data left");
        *s.fail_import_at.lock().unwrap() = None;
        assert_eq!(s.import_layer(&src, "partial").unwrap(), 2);
        s.close().unwrap();

        let s = Storage::open(d.path().join("store"), cfg()).unwrap();
        let p = s.layer("imp").unwrap();
        assert_eq!(read_file(&p, "saves/ONE.ess"), b"one");
    }

    #[test]
    fn delete_refuses_a_live_layer_and_frees_it_otherwise() {
        let (s, _d) = temp_storage();
        let x = s.layer("x").unwrap();
        write_file(&x, "f.bin", &vec![1u8; 2 * BS as usize + 1]);
        write_file(&x, "dir/g.bin", b"gg");
        let keep = s.layer("keep").unwrap();
        write_file(&keep, "k", b"k");
        drop(keep);
        assert_eq!(
            s.layers().unwrap(),
            vec![
                LayerInfo {
                    name: "keep".into(),
                    files: 1,
                    logical_bytes: 1
                },
                LayerInfo {
                    name: "x".into(),
                    files: 2,
                    logical_bytes: 2 * BS + 3
                },
            ]
        );
        let lid = s.catalog.layer_id("x").unwrap().unwrap();
        let guids: Vec<_> = s
            .catalog
            .all_layer_guids()
            .unwrap()
            .into_iter()
            .filter(|(l, ..)| *l == lid)
            .map(|(.., g)| g)
            .collect();
        assert_eq!(guids.len(), 2);

        assert!(matches!(
            s.delete_layer("x"),
            Err(StorageError::LayerInUse(_))
        ));
        drop(x);
        s.delete_layer("x").unwrap();
        assert_eq!(
            s.layers()
                .unwrap()
                .into_iter()
                .map(|l| l.name)
                .collect::<Vec<_>>(),
            vec!["keep".to_string()]
        );
        for g in &guids {
            assert!(s.store.stat(&layer_file_id(g)).unwrap().is_none());
        }
        assert!(matches!(
            s.delete_layer("x"),
            Err(StorageError::NoSuchLayer(_))
        ));
        let st = s.stats();
        assert_eq!(st.layer_count, 1);
        assert!(st.pack_bytes >= st.live_bytes);
        assert!(st.live_bytes > 0);
    }

    /// A provider whose last reference is gone but whose `Drop` (a final
    /// commit and durable point) is still running keeps its layer in use, and
    /// a new `layer()` call waits for it rather than building a second one.
    #[test]
    fn a_provider_mid_drop_keeps_its_layer_in_use() {
        let (s, _d) = temp_storage();
        let x = s.layer("x").unwrap();
        write_file(&x, "f", b"f");
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        *s.drop_hook.lock().unwrap() = Some(Box::new(move || {
            entered_tx.send(()).unwrap();
            go_rx.recv().unwrap();
        }));
        let t = std::thread::spawn(move || drop(x));
        entered_rx.recv().unwrap();
        assert_eq!(s.layers_in_use(), vec!["x".to_string()]);
        assert!(matches!(
            s.delete_layer("x"),
            Err(StorageError::LayerInUse(_))
        ));
        let s2 = Arc::clone(&s);
        let reopened = std::thread::spawn(move || s2.layer("x").map(|_| ()));
        go_tx.send(()).unwrap();
        t.join().unwrap();
        reopened.join().unwrap().unwrap();
        assert!(s.layers_in_use().is_empty());
        s.delete_layer("x").unwrap();
    }

    #[test]
    fn host_components_are_single_plain_names() {
        use super::host_component;
        for ok in ["a.txt", "Saves", "..x", "x..", ".hidden", "sp ace"] {
            assert_eq!(host_component(ok).unwrap(), ok);
        }
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "/abs",
            "\\abs",
            "C:",
            "C:evil",
            "C:\\evil",
            "a:b",
            "\\\\srv\\share",
        ] {
            assert!(
                matches!(host_component(bad), Err(StorageError::BadRequest(_))),
                "{bad:?} accepted"
            );
        }
    }

    /// A catalog name with a drive prefix cannot make an export write outside
    /// its target (on Windows, `dir.join("C:evil")` would replace `dir`).
    #[test]
    fn export_refuses_a_drive_prefixed_name() {
        let (s, d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "ok.txt", b"ok");
        drop(p);
        let lid = s.catalog.layer_id("l").unwrap().unwrap();
        let guid = crate::ids::new_guid();
        s.store.set_len(&layer_file_id(&guid), 0).unwrap();
        s.catalog
            .put(
                lid,
                "c:evil",
                &crate::catalog::EntryRec {
                    name: "C:evil".into(),
                    kind: KIND_FILE,
                    guid,
                    len: 0,
                    mtime: 0,
                },
                false,
            )
            .unwrap();
        let out = d.path().join("out");
        assert!(matches!(
            s.export_layer("l", &out),
            Err(StorageError::BadRequest(_))
        ));
        assert!(!d.path().join("C:evil").exists());
        assert!(disk_names(&out).iter().all(|n| !n.contains("evil")));
    }

    /// Two host names that differ only in case would be one layer entry.
    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn import_refuses_names_that_differ_only_in_case() {
        let (s, d) = temp_storage();
        let src = d.path().join("in");
        std::fs::create_dir_all(src.join("Sub")).unwrap();
        std::fs::write(src.join("first.txt"), b"1").unwrap();
        std::fs::write(src.join("Sub/Save.ess"), b"A").unwrap();
        std::fs::write(src.join("Sub/save.ess"), b"B").unwrap();
        match s.import_layer(&src, "cases") {
            Err(StorageError::BadRequest(m)) => assert!(m.contains("differ only in case"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(s.layers().unwrap().iter().all(|l| l.name != "cases"));
    }

    #[cfg(unix)]
    #[test]
    fn import_skips_broken_symlinks() {
        let (s, d) = temp_storage();
        let src = d.path().join("in");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("real.txt"), b"real").unwrap();
        std::os::unix::fs::symlink(src.join("nowhere"), src.join("dangling")).unwrap();
        std::os::unix::fs::symlink(src.join("real.txt"), src.join("link.txt")).unwrap();
        assert_eq!(s.import_layer(&src, "links").unwrap(), 2);
        let p = s.layer("links").unwrap();
        assert!(p.getattr(at("dangling")).unwrap().is_none());
        assert_eq!(read_file(&p, "link.txt"), b"real");
    }

    /// A provider whose `Drop` panics still leaves the registry, so the layer
    /// is not in use forever and `layer()` does not wait forever.
    #[test]
    fn a_panicking_provider_drop_still_leaves_the_registry() {
        let (s, _d) = temp_storage();
        let x = s.layer("x").unwrap();
        *s.drop_hook.lock().unwrap() = Some(Box::new(|| panic!("injected drop panic")));
        assert!(std::thread::spawn(move || drop(x)).join().is_err());
        assert!(s.layers_in_use().is_empty());
        s.layer("x").unwrap();
    }
}
