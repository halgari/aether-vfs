//! The session's registry layer: attaching `overlay.reg`'s provider and the
//! session-end durable point.

use std::sync::Arc;

use vfs_director::registry::{DurableSync, RegistryHost};
use vfs_provider::Provider;

use super::Session;

impl Session {
    /// Attach (`Some`) or detach (`None`) the session's registry layer: a
    /// provider holding `overlay.reg`. While one is attached, launches set
    /// [`vfs_env::REGISTRY`] (`VFS_REGISTRY=1`) so injected processes install
    /// the registry hooks; without it virtualisation is off.
    ///
    /// **`sync`: the durable point of the layer's store** (registry overlay
    /// spec §5). Saves of the overlay are ordinary writes to the layer, which
    /// a deferred-durability store does not make durable by itself. Pass the
    /// owning store's sync — for a layer of `aether-storage`'s `Storage`,
    /// `aether_storage::registry_sync_for(&storage)` — and the session calls it after
    /// the final save when it stops serving ([`Session::stop_serve`], and so
    /// on drop) and when the layer is replaced or detached, and the saver
    /// calls it at most every five minutes while a save is not yet durable.
    /// A crash after the game exits then keeps the session's registry writes.
    /// `None` leaves durability to the store's own policy. Ignored when
    /// detaching.
    ///
    /// Attaching opens a [`RegistryHost`] on the layer; a layer that is
    /// already attached is made durable first (saved, then `sync`), then
    /// replaced. Detaching does the same here rather than leaving the save to
    /// whichever thread drops its last reference. Errors are the host's or the
    /// hook's status codes, as [`Session::set_write_layer`]'s are; a failure
    /// leaves the old layer attached.
    ///
    /// **Between launches only.** It takes effect for launches that start
    /// after it returns. A process already running keeps the registry hooks
    /// it installed at start: detaching (or replacing) the layer while one
    /// runs makes that process's registry writes fail
    /// (`STATUS_UNSUCCESSFUL`) and its overlay disappear from its view (it
    /// sees the real registry, or the new layer). Hosts attach and detach only
    /// while no launch of the session is running.
    pub fn set_registry_layer(
        &self,
        layer: Option<Arc<dyn Provider>>,
        sync: Option<RegistrySync>,
    ) -> Result<(), i32> {
        let host = layer
            .map(|l| RegistryHost::open_with_sync(l, sync))
            .transpose()?;
        let old = self.kernel.registry();
        // Made durable before the swap: nothing of the old layer may be lost,
        // and a failure leaves the old host attached.
        if let Some(old) = &old {
            old.durable()?;
        }
        self.kernel.set_registry(host);
        // `VFS_REGISTRY` reaches a child through `launch`: `apply_env_roots`
        // sets or clears it, under `LAUNCH_ENV_LOCK`, from
        // `registry_attached()` on every serve and launch (a unix launch
        // builds its own environment, `WineLaunch::registry`). So this does
        // not write process env.
        // The old host is dropped here, already clean, so its drop-time save
        // writes nothing.
        drop(old);
        Ok(())
    }

    /// Whether a registry layer is attached ([`Session::set_registry_layer`]).
    pub(super) fn registry_attached(&self) -> bool {
        self.kernel.registry().is_some()
    }

    /// The session-end durable point of the attached registry overlay: save,
    /// then the layer's sync hook ([`Session::set_registry_layer`]), logging a
    /// failure: stop and drop have no caller to report one to.
    pub(super) fn flush_registry(&self) {
        if let Some(host) = self.kernel.registry() {
            if let Err(st) = host.durable() {
                tracing::error!(
                    status = st,
                    "registry overlay: durable point at stop failed"
                );
            }
        }
    }
}

/// The durable point of a registry layer's store, for
/// [`Session::set_registry_layer`]: everything written to the layer so far
/// is durable when it returns `Ok`; `Err` is a ring status. Called from the
/// session's registry saver thread and at session stop; it must not call
/// back into the session.
pub type RegistrySync = DurableSync;

#[cfg(test)]
mod registry_layer_tests {
    use super::*;
    #[cfg(unix)]
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::path::PathBuf;
    #[cfg(unix)]
    use vfs_proton::launch::{LaunchFiles, RingGeometry, WineLaunch};
    use vfs_provider::VPath;
    use vfs_provider::OPEN_READ;

    const KEY: &str = r"\Registry\Machine\Software\Mod";

    fn read(p: &Arc<dyn Provider>, name: &str) -> Option<Vec<u8>> {
        let (h, _, _) = p.open(VPath::at_default(name), OPEN_READ).ok()?;
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = p.read_at(h, out.len() as u64, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        p.close(h).unwrap();
        Some(out)
    }

    /// The launch environment for what the session currently says about its
    /// registry layer (the same `registry_attached()` `launch` passes).
    #[cfg(unix)]
    fn launch_env_of(s: &Session) -> BTreeMap<String, String> {
        let p = |n: &str| PathBuf::from(format!("/x/{n}"));
        let mut l = WineLaunch::new(
            p("rt"),
            p("pfx"),
            r"C:\t.exe".to_string(),
            r"C:\m".to_string(),
            LaunchFiles {
                injector: p("inj"),
                shim_dll: p("shim"),
                config_file: p("cfg"),
                ready_file: p("ready"),
            },
            RingGeometry {
                path: p("ring"),
                host_path: None,
                bytes: 1,
                arena_offset: 1,
                arena_len: 1,
                payload_cap: 1,
            },
        );
        l.registry = s.registry_attached();
        vfs_proton::launch::launch_env(&l)
    }

    /// A registry layer. The tests against `aether-storage`'s layers (and its
    /// durable point) are in that crate's `tests/registry_layer.rs`.
    fn memory_layer() -> Arc<dyn Provider> {
        Arc::new(crate::MemoryProvider::new())
    }

    #[test]
    fn attach_and_detach_toggle_the_registry_flag() {
        let layer = memory_layer();
        let s = Session::new();
        assert!(!s.registry_attached());
        s.set_registry_layer(Some(layer), None).unwrap();
        assert!(s.registry_attached());
        assert!(s.kernel().registry().is_some());
        #[cfg(unix)]
        assert_eq!(
            launch_env_of(&s).get("VFS_REGISTRY").map(String::as_str),
            Some("1")
        );
        s.set_registry_layer(None, None).unwrap();
        assert!(!s.registry_attached());
        assert!(s.kernel().registry().is_none());
        #[cfg(unix)]
        assert!(!launch_env_of(&s).contains_key("VFS_REGISTRY"));
    }

    #[test]
    fn stop_flushes_and_a_second_save_replaces_the_first() {
        let layer = memory_layer();
        let mut s = Session::new();
        s.set_registry_layer(Some(layer.clone()), None).unwrap();
        let host = s.kernel().registry().unwrap();

        host.set_value(KEY, "v", 1, b"one\0").unwrap();
        s.stop_serve();
        let first = read(&layer, "overlay.reg").expect("stop must leave overlay.reg in the layer");
        assert!(first.windows(3).any(|w| w == b"one"));

        // Rename onto an existing overlay.reg.
        host.set_value(KEY, "v", 1, b"two\0").unwrap();
        s.stop_serve();
        let second = read(&layer, "overlay.reg").unwrap();
        assert!(second.windows(3).any(|w| w == b"two"));
        assert!(!second.windows(3).any(|w| w == b"one"));
        assert!(
            read(&layer, "overlay.reg.tmp").is_none(),
            "the temp file must be renamed away"
        );

        // Replacing the layer flushes the old one first.
        host.set_value(KEY, "v", 1, b"six\0").unwrap();
        s.set_registry_layer(Some(memory_layer()), None).unwrap();
        assert!(read(&layer, "overlay.reg")
            .unwrap()
            .windows(3)
            .any(|w| w == b"six"));
    }
}
