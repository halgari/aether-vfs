//! Registry probe for the registry overlay's end-to-end test (registry overlay spec §8.3).
//!
//! `vfs-fixture-registry.exe <mode> [run id]` drives the registry through Win32 only, under
//! `Software\AetherVfsRegistryTest` in both HKCU and HKLM, and prints one `reg: ` line per
//! call: the call, its status, the sizes it returned and the data as hex. The same script
//! run with the overlay off and on must print the same lines, so nothing printed may be
//! something Wine and the overlay legitimately differ on:
//!
//! * no absolute key names (hive prefixes differ in case), only names relative to the
//!   scratch root, as the fixture wrote them or as an enumeration returned them;
//! * no last-write times and no security descriptor sizes;
//! * enumerations are printed **sorted by name, case-insensitively**, with the index at which
//!   they ended. The order itself is not compared: Wine keeps values and subkeys sorted,
//!   Windows keeps values in insertion order, and the overlay (spec §3.3) lists its own
//!   values first and its created subkeys after the real ones. Every entry, its count and
//!   the end status are compared.
//!
//! Modes:
//!
//! * `cleanup` — delete the scratch root in both hives;
//! * `prepare` — create `Base` (three values, subkeys `Child` and `Keep`), the real key the
//!   copy-on-write steps of `run` write through;
//! * `run` — the scripted sequence, under `<root>\<run id>` and through a handle on `Base`
//!   opened before anything else. It leaves its keys in place, for `probe`;
//! * `probe` — dump everything under the scratch root: per key, `RegQueryInfoKeyW`'s counts
//!   and longest lengths, every value and every subkey.
//!
//! Exit code 0 when the mode ran (a failed registry call is data, not a failure), 12 for an
//! unknown mode. Not 2 or 3, which are `vfs-injector`'s own.
#![allow(unsafe_code)]

#[cfg(not(windows))]
fn main() {
    eprintln!("vfs-fixture-registry is a Windows program; build it with bin/build-windows");
    std::process::exit(11);
}

#[cfg(windows)]
fn main() {
    std::process::exit(script::main());
}

#[cfg(windows)]
mod script {
    use std::io::Write;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteTreeW, RegDeleteValueW,
        RegEnumKeyExW, RegEnumValueW, RegNotifyChangeKeyValue, RegOpenKeyExW, RegQueryInfoKeyW,
        RegQueryValueExW, RegRenameKey, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        HKEY_LOCAL_MACHINE, KEY_ALL_ACCESS, KEY_READ, REG_NOTIFY_CHANGE_LAST_SET,
        REG_NOTIFY_CHANGE_NAME, REG_OPTION_NON_VOLATILE,
    };
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    const ROOT: &str = r"Software\AetherVfsRegistryTest";
    const BASE: &str = "Base";

    const REG_NONE: u32 = 0;
    const REG_SZ: u32 = 1;
    const REG_EXPAND_SZ: u32 = 2;
    const REG_BINARY: u32 = 3;
    const REG_DWORD: u32 = 4;
    const REG_MULTI_SZ: u32 = 7;
    const REG_QWORD: u32 = 11;
    /// A type number no API defines: the overlay must keep it as raw bytes.
    const REG_ODD: u32 = 0x1234;

    pub fn main() -> i32 {
        let args: Vec<String> = std::env::args().collect();
        let mode = args.get(1).map(String::as_str).unwrap_or("");
        let run = args.get(2).map(String::as_str).unwrap_or("run");
        line(format!("mode {mode}"));
        for (label, hk) in [("HKCU", HKEY_CURRENT_USER), ("HKLM", HKEY_LOCAL_MACHINE)] {
            let t = T { tag: label };
            match mode {
                "cleanup" => t.cleanup(hk),
                "prepare" => t.prepare(hk),
                "run" => t.run(hk, run),
                "probe" => t.probe(hk),
                _ => {
                    line(format!("unknown mode {mode:?}"));
                    return 12;
                }
            }
        }
        line("end".to_string());
        0
    }

    // -----------------------------------------------------------------------------------
    // Output
    // -----------------------------------------------------------------------------------

    fn line(s: String) {
        // One write per line: Wine's own stderr shares the log this ends up in.
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(format!("reg: {s}\n").as_bytes());
        let _ = out.flush();
    }

    fn hex(b: &[u8]) -> String {
        if b.is_empty() {
            return "-".to_string();
        }
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// A value or key name as printed: quoted, and a long run of one character shortened.
    fn shown(name: &str) -> String {
        let mut chars = name.chars();
        match chars.next() {
            Some(c) if name.chars().count() > 40 && chars.all(|d| d == c) => {
                format!("\"{c}\"*{}", name.chars().count())
            }
            _ => format!("{name:?}"),
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn utf16(b: &[u16]) -> String {
        String::from_utf16_lossy(b)
    }

    fn sz(s: &str) -> Vec<u8> {
        wide(s).iter().flat_map(|c| c.to_le_bytes()).collect()
    }

    fn multi(items: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for i in items {
            for c in i.encode_utf16() {
                out.extend_from_slice(&c.to_le_bytes());
            }
            out.extend_from_slice(&[0, 0]);
        }
        out.extend_from_slice(&[0, 0]);
        out
    }

    fn long_name() -> String {
        "L".repeat(300)
    }

    /// An open key, closed on drop.
    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            unsafe { RegCloseKey(self.0) };
        }
    }

    /// One enumerated value.
    struct Val {
        name: String,
        cch: u32,
        ty: u32,
        data: Vec<u8>,
    }

    /// One change notification: what it is for, `RegNotifyChangeKeyValue`'s subtree flag and
    /// filter, and how long to wait for it.
    struct Watch {
        what: &'static str,
        tree: bool,
        filter: u32,
        ms: u32,
    }

    /// The script for one hive; `tag` starts every line.
    struct T {
        tag: &'static str,
    }

    impl T {
        fn p(&self, s: String) {
            line(format!("{} {s}", self.tag));
        }

        // -------------------------------------------------------------------------------
        // Logged calls
        // -------------------------------------------------------------------------------

        fn create(&self, parent: HKEY, at: &str, sub: &str) -> Option<Key> {
            let mut k: HKEY = null_mut();
            let mut disp = 0u32;
            let st = unsafe {
                RegCreateKeyExW(
                    parent,
                    wide(sub).as_ptr(),
                    0,
                    null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_ALL_ACCESS,
                    null(),
                    &mut k,
                    &mut disp,
                )
            };
            self.p(format!(
                "CreateKey [{at}] {} -> {st} disp={disp}",
                shown(sub)
            ));
            (st == 0).then_some(Key(k))
        }

        fn open(&self, parent: HKEY, at: &str, sub: &str, access: u32) -> Option<Key> {
            let mut k: HKEY = null_mut();
            let st = unsafe { RegOpenKeyExW(parent, wide(sub).as_ptr(), 0, access, &mut k) };
            self.p(format!(
                "OpenKey [{at}] {} access={access:#x} -> {st}",
                shown(sub)
            ));
            (st == 0).then_some(Key(k))
        }

        fn set(&self, k: HKEY, at: &str, name: &str, ty: u32, data: &[u8]) {
            let st = unsafe {
                RegSetValueExW(
                    k,
                    wide(name).as_ptr(),
                    0,
                    ty,
                    if data.is_empty() {
                        null()
                    } else {
                        data.as_ptr()
                    },
                    data.len() as u32,
                )
            };
            self.p(format!(
                "SetValue [{at}] {} type={ty:#x} cb={} -> {st}",
                shown(name),
                data.len()
            ));
        }

        /// `RegQueryValueExW` with no buffer (`None`: the size query) or a buffer of `buf`
        /// bytes. The data is printed only on success: on `ERROR_MORE_DATA` the buffer's
        /// contents are undefined.
        fn query(&self, k: HKEY, at: &str, name: &str, buf: Option<u32>) {
            let mut ty = 0xDEAD_u32;
            let mut cb = buf.unwrap_or(0);
            let mut data = vec![0xCCu8; buf.unwrap_or(0) as usize];
            let st = unsafe {
                RegQueryValueExW(
                    k,
                    wide(name).as_ptr(),
                    null(),
                    &mut ty,
                    if buf.is_some() {
                        data.as_mut_ptr()
                    } else {
                        null_mut()
                    },
                    &mut cb,
                )
            };
            let shown_buf = buf.map_or("null".to_string(), |b| b.to_string());
            let mut s = format!(
                "QueryValue [{at}] {} buf={shown_buf} -> {st} type={ty:#x} cb={cb}",
                shown(name)
            );
            if st == 0 && buf.is_some() {
                s.push_str(&format!(" data={}", hex(&data[..cb as usize])));
            }
            self.p(s);
        }

        fn delete_value(&self, k: HKEY, at: &str, name: &str) {
            let st = unsafe { RegDeleteValueW(k, wide(name).as_ptr()) };
            self.p(format!("DeleteValue [{at}] {} -> {st}", shown(name)));
        }

        fn delete_key(&self, k: HKEY, at: &str, sub: &str) {
            let st = unsafe { RegDeleteKeyW(k, wide(sub).as_ptr()) };
            self.p(format!("DeleteKey [{at}] {} -> {st}", shown(sub)));
        }

        fn rename(&self, k: HKEY, at: &str, sub: &str, new: &str) {
            let st = unsafe { RegRenameKey(k, wide(sub).as_ptr(), wide(new).as_ptr()) };
            self.p(format!(
                "RenameKey [{at}] {} -> {} -> {st}",
                shown(sub),
                shown(new)
            ));
        }

        fn info(&self, k: HKEY, at: &str) {
            let (mut subs, mut max_sub, mut max_class) = (0u32, 0u32, 0u32);
            let (mut vals, mut max_name, mut max_data) = (0u32, 0u32, 0u32);
            // With a class buffer: Wine's advapi copies the class from where the reply says.
            let mut class = [0u16; 64];
            let mut class_cch = class.len() as u32;
            let st = unsafe {
                RegQueryInfoKeyW(
                    k,
                    class.as_mut_ptr(),
                    &mut class_cch,
                    null(),
                    &mut subs,
                    &mut max_sub,
                    &mut max_class,
                    &mut vals,
                    &mut max_name,
                    &mut max_data,
                    null_mut(),
                    null_mut(),
                )
            };
            self.p(format!(
                "QueryInfoKey [{at}] -> {st} class={:?} subkeys={subs} max_subkey={max_sub} \
                 max_class={max_class} values={vals} max_value_name={max_name} \
                 max_value_data={max_data}",
                utf16(&class[..(class_cch as usize).min(class.len())])
            ));
        }

        /// Every subkey by index until the enumeration ends. Returns the names in index
        /// order (for the small-buffer calls), prints them sorted.
        fn enum_keys(&self, k: HKEY, at: &str, print: bool) -> Vec<String> {
            let mut names = Vec::new();
            let mut end = (0u32, 0u32);
            for i in 0..1000u32 {
                let mut buf = [0u16; 256];
                let mut cch = buf.len() as u32;
                let mut class = [0u16; 64];
                let mut class_cch = class.len() as u32;
                let st = unsafe {
                    RegEnumKeyExW(
                        k,
                        i,
                        buf.as_mut_ptr(),
                        &mut cch,
                        null(),
                        class.as_mut_ptr(),
                        &mut class_cch,
                        null_mut(),
                    )
                };
                if st != 0 {
                    end = (i, st);
                    break;
                }
                names.push((utf16(&buf[..cch as usize]), cch, class_cch));
            }
            if print {
                let mut sorted = names.clone();
                sorted.sort_by_key(|(n, _, _)| n.to_lowercase());
                for (n, cch, class_cch) in &sorted {
                    self.p(format!(
                        "EnumKey [{at}] {} cch={cch} class_cch={class_cch}",
                        shown(n)
                    ));
                }
                self.p(format!("EnumKey [{at}] end index={} -> {}", end.0, end.1));
            }
            names.into_iter().map(|(n, _, _)| n).collect()
        }

        /// Every value by index until the enumeration ends, printed sorted by name.
        fn enum_values(&self, k: HKEY, at: &str, print: bool) -> Vec<String> {
            let mut vals = Vec::new();
            let mut end = (0u32, 0u32);
            for i in 0..1000u32 {
                let mut name = vec![0u16; 16_384];
                let mut cch = name.len() as u32;
                let mut data = vec![0u8; 4096];
                let mut cb = data.len() as u32;
                let mut ty = 0u32;
                let st = unsafe {
                    RegEnumValueW(
                        k,
                        i,
                        name.as_mut_ptr(),
                        &mut cch,
                        null(),
                        &mut ty,
                        data.as_mut_ptr(),
                        &mut cb,
                    )
                };
                if st != 0 {
                    end = (i, st);
                    break;
                }
                vals.push(Val {
                    name: utf16(&name[..cch as usize]),
                    cch,
                    ty,
                    data: data[..cb as usize].to_vec(),
                });
            }
            let order: Vec<String> = vals.iter().map(|v| v.name.clone()).collect();
            if print {
                vals.sort_by_key(|v| v.name.to_lowercase());
                for v in &vals {
                    self.p(format!(
                        "EnumValue [{at}] {} cch={} type={:#x} cb={} data={}",
                        shown(&v.name),
                        v.cch,
                        v.ty,
                        v.data.len(),
                        hex(&v.data)
                    ));
                }
                self.p(format!("EnumValue [{at}] end index={} -> {}", end.0, end.1));
            }
            order
        }

        /// `RegEnumValueW` at the index of `target` with a one-byte data buffer, then with a
        /// two-character name buffer: `ERROR_MORE_DATA` and the sizes, whatever the order.
        fn enum_value_small(&self, k: HKEY, at: &str, target: &str) {
            let order = self.enum_values(k, at, false);
            let Some(i) = order.iter().position(|n| n.eq_ignore_ascii_case(target)) else {
                self.p(format!("EnumValueSmall [{at}] {} missing", shown(target)));
                return;
            };
            let mut name = vec![0u16; 16_384];
            let mut cch = name.len() as u32;
            let mut data = [0u8; 1];
            let mut cb = 1u32;
            let mut ty = 0u32;
            let st = unsafe {
                RegEnumValueW(
                    k,
                    i as u32,
                    name.as_mut_ptr(),
                    &mut cch,
                    null(),
                    &mut ty,
                    data.as_mut_ptr(),
                    &mut cb,
                )
            };
            self.p(format!(
                "EnumValue [{at}] {} databuf=1 -> {st} cch={cch} type={ty:#x} cb={cb}",
                shown(target)
            ));
            let mut small = [0u16; 2];
            let mut cch = 2u32;
            let mut cb = 0u32;
            let st = unsafe {
                RegEnumValueW(
                    k,
                    i as u32,
                    small.as_mut_ptr(),
                    &mut cch,
                    null(),
                    null_mut(),
                    null_mut(),
                    &mut cb,
                )
            };
            self.p(format!(
                "EnumValue [{at}] {} namebuf=2 -> {st} cch={cch} cb={cb}",
                shown(target)
            ));
        }

        /// `RegEnumKeyExW` at the index of `target` with a two-character name buffer.
        fn enum_key_small(&self, k: HKEY, at: &str, target: &str) {
            let order = self.enum_keys(k, at, false);
            let Some(i) = order.iter().position(|n| n.eq_ignore_ascii_case(target)) else {
                self.p(format!("EnumKeySmall [{at}] {} missing", shown(target)));
                return;
            };
            let mut small = [0u16; 2];
            let mut cch = 2u32;
            let st = unsafe {
                RegEnumKeyExW(
                    k,
                    i as u32,
                    small.as_mut_ptr(),
                    &mut cch,
                    null(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                )
            };
            self.p(format!(
                "EnumKey [{at}] {} namebuf=2 -> {st} cch={cch}",
                shown(target)
            ));
        }

        /// Register an asynchronous change notification on `k`, run `then`, and wait up to
        /// `ms` for the event.
        fn notify(&self, k: HKEY, at: &str, w: Watch, then: impl FnOnce()) {
            let Watch {
                what,
                tree,
                filter,
                ms,
            } = w;
            let ev = unsafe { CreateEventW(null(), 1, 0, null()) };
            if ev.is_null() {
                self.p(format!("Notify [{at}] {what}: CreateEventW failed"));
                return;
            }
            let st = unsafe { RegNotifyChangeKeyValue(k, tree as i32, filter, ev, 1) };
            self.p(format!(
                "Notify [{at}] {what} tree={tree} filter={filter:#x} -> {st}"
            ));
            then();
            let w = unsafe { WaitForSingleObject(ev, ms) };
            let outcome = match w {
                WAIT_OBJECT_0 => "signalled".to_string(),
                WAIT_TIMEOUT => "timeout".to_string(),
                other => format!("wait {other:#x}"),
            };
            self.p(format!("Notify [{at}] {what} -> {outcome}"));
            unsafe { CloseHandle(ev) };
        }

        // -------------------------------------------------------------------------------
        // Modes
        // -------------------------------------------------------------------------------

        fn cleanup(&self, hk: HKEY) {
            let root = wide(ROOT);
            let tree = unsafe { RegDeleteTreeW(hk, root.as_ptr()) };
            let key = unsafe { RegDeleteKeyW(hk, root.as_ptr()) };
            self.p(format!("cleanup tree={tree} key={key}"));
        }

        fn prepare(&self, hk: HKEY) {
            let Some(base) = self.create(hk, "", &format!(r"{ROOT}\{BASE}")) else {
                return;
            };
            let b = base.0;
            self.set(b, BASE, "Name", REG_SZ, &sz("base"));
            self.set(b, BASE, "Count", REG_DWORD, &7u32.to_le_bytes());
            self.set(b, BASE, "Blob", REG_BINARY, &[1, 2, 3]);
            if let Some(child) = self.create(b, BASE, "Child") {
                self.set(child.0, "Base\\Child", "Inner", REG_SZ, &sz("x"));
            }
            self.create(b, BASE, "Keep");
        }

        fn probe(&self, hk: HKEY) {
            match self.open(hk, "", ROOT, KEY_READ) {
                Some(root) => self.dump(root.0, "."),
                None => self.p("probe: no scratch root".to_string()),
            }
        }

        fn dump(&self, k: HKEY, at: &str) {
            self.info(k, at);
            self.enum_values(k, at, true);
            let mut subs = self.enum_keys(k, at, true);
            subs.sort_by_key(|n| n.to_lowercase());
            for s in subs {
                let child_at = if at == "." {
                    s.clone()
                } else {
                    format!(r"{at}\{s}")
                };
                if let Some(c) = self.open(k, at, &s, KEY_READ) {
                    self.dump(c.0, &child_at);
                }
            }
        }

        fn run(&self, hk: HKEY, run: &str) {
            // The copy-on-write handle: a real key, opened before anything was written, so
            // with the overlay on it starts as a pass-through handle.
            let base = self.open(hk, "", &format!(r"{ROOT}\{BASE}"), KEY_ALL_ACCESS);
            if let Some(b) = &base {
                self.query(b.0, BASE, "Name", Some(64));
            }

            let path = format!(r"{ROOT}\{run}");
            let Some(s) = self.create(hk, "", &path) else {
                return;
            };
            let k = s.0;
            let at = "S";
            // Again: opened, not created.
            drop(self.create(hk, "", &path));

            // Every value type, the default value, empty data and a long name.
            let long = long_name();
            let values: Vec<(&str, u32, Vec<u8>)> = vec![
                ("", REG_SZ, sz("default value")),
                ("sz", REG_SZ, sz("hello")),
                ("expand", REG_EXPAND_SZ, sz(r"%SystemRoot%\x")),
                ("bin", REG_BINARY, vec![0, 1, 2, 0xFF]),
                ("dword", REG_DWORD, 0x1234_5678u32.to_le_bytes().to_vec()),
                (
                    "qword",
                    REG_QWORD,
                    0x0102_0304_0506_0708u64.to_le_bytes().to_vec(),
                ),
                ("multi", REG_MULTI_SZ, multi(&["a", "bc"])),
                ("none", REG_NONE, vec![1, 2]),
                ("odd", REG_ODD, vec![9, 8, 7]),
                ("empty", REG_BINARY, vec![]),
                ("emptysz", REG_SZ, vec![]),
                (long.as_str(), REG_DWORD, 1u32.to_le_bytes().to_vec()),
                ("Mixed Case Name", REG_SZ, sz("MiXeD")),
                (
                    "big",
                    REG_BINARY,
                    (0..600u32).map(|i| (i * 7) as u8).collect(),
                ),
            ];
            for (name, ty, data) in &values {
                self.set(k, at, name, *ty, data);
            }
            // Overwrite one.
            self.set(k, at, "dword", REG_DWORD, &0xCAFEu32.to_le_bytes());
            for (name, _, _) in &values {
                self.query(k, at, name, None);
                self.query(k, at, name, Some(4096));
            }
            // Too small, exactly enough, other spellings, missing.
            self.query(k, at, "sz", Some(2));
            self.query(k, at, "sz", Some(12));
            self.query(k, at, "big", Some(599));
            self.query(k, at, "SZ", Some(64));
            self.query(k, at, "mixed case name", Some(64));
            self.query(k, at, "missing", Some(64));
            self.query(k, at, "missing", None);

            // A handle without write access cannot write.
            if let Some(ro) = self.open(hk, "", &path, KEY_READ) {
                self.set(ro.0, "S(read)", "denied", REG_SZ, &sz("no"));
                self.query(ro.0, "S(read)", "sz", Some(64));
            }

            // Subkeys, mixed with the values above.
            for sub in ["Sub1", "sub2", "Zeta", "alpha", r"Sub1\Deep"] {
                drop(self.create(k, at, sub));
            }
            if let Some(c) = self.open(k, at, "sub2", KEY_ALL_ACCESS) {
                self.set(c.0, "S\\sub2", "v", REG_SZ, &sz("two"));
            }
            if let Some(c) = self.open(k, at, "alpha", KEY_ALL_ACCESS) {
                self.set(c.0, "S\\alpha", "a", REG_DWORD, &1u32.to_le_bytes());
            }
            if let Some(c) = self.open(k, at, r"Sub1\Deep", KEY_ALL_ACCESS) {
                self.set(c.0, r"S\Sub1\Deep", "d", REG_SZ, &sz("deep"));
            }
            self.open(k, at, "SUB2", KEY_READ);
            self.enum_keys(k, at, true);
            self.enum_values(k, at, true);
            self.info(k, at);
            self.enum_value_small(k, at, "sz");
            self.enum_key_small(k, at, "alpha");

            // Deletes.
            self.delete_value(k, at, "none");
            self.delete_value(k, at, "none");
            self.query(k, at, "none", Some(64));
            self.delete_value(k, at, "EMPTY");
            self.delete_key(k, at, "Sub1");
            self.delete_key(k, at, r"Sub1\Deep");
            self.delete_key(k, at, "Sub1");
            self.open(k, at, "Sub1", KEY_READ);
            self.delete_key(k, at, "nothere");

            // Rename.
            self.rename(k, at, "sub2", "Renamed");
            if let Some(c) = self.open(k, at, "Renamed", KEY_READ) {
                self.query(c.0, "S\\Renamed", "v", Some(64));
            }
            self.open(k, at, "sub2", KEY_READ);
            self.rename(k, at, "nothere", "Other");
            self.rename(k, at, "Renamed", "Zeta");

            // Recreate a deleted key: it comes back empty.
            if let Some(c) = self.create(k, at, "Sub1") {
                self.info(c.0, "S\\Sub1");
                self.enum_keys(c.0, "S\\Sub1", true);
            }

            // Notifications.
            let watch = |what, tree, filter, ms| Watch {
                what,
                tree,
                filter,
                ms,
            };
            if let Some(n) = self.open(k, at, "", KEY_READ) {
                let w = watch("value", false, REG_NOTIFY_CHANGE_LAST_SET, 5000);
                self.notify(n.0, at, w, || {
                    self.set(k, at, "notified", REG_SZ, &sz("yes"))
                });
            }
            if let Some(n) = self.open(k, at, "", KEY_READ) {
                let w = watch("subkey value", true, REG_NOTIFY_CHANGE_LAST_SET, 5000);
                self.notify(n.0, at, w, || {
                    if let Some(c) = self.open(k, at, "alpha", KEY_ALL_ACCESS) {
                        self.set(c.0, "S\\alpha", "b", REG_DWORD, &2u32.to_le_bytes());
                    }
                });
            }
            if let Some(n) = self.open(k, at, "", KEY_READ) {
                let w = watch("subkey create", true, REG_NOTIFY_CHANGE_NAME, 5000);
                self.notify(n.0, at, w, || drop(self.create(k, at, r"alpha\Fresh")));
            }
            // A change beside the watched key, not in it: nothing to report.
            if let Some(n) = self.open(k, at, "alpha", KEY_READ) {
                let w = watch("sibling write", false, REG_NOTIFY_CHANGE_LAST_SET, 1000);
                self.notify(n.0, "S\\alpha", w, || {
                    if let Some(c) = self.open(k, at, "Zeta", KEY_ALL_ACCESS) {
                        self.set(c.0, "S\\Zeta", "z", REG_SZ, &sz("zz"));
                    }
                });
            }

            self.enum_keys(k, at, true);
            self.enum_values(k, at, true);
            self.info(k, at);

            // Copy-on-write through the handle opened at the start.
            let Some(b) = base else { return };
            let b = b.0;
            self.query(b, BASE, "Name", Some(64));
            self.set(b, BASE, "Added", REG_SZ, &sz("new"));
            self.set(b, BASE, "Count", REG_DWORD, &8u32.to_le_bytes());
            self.delete_value(b, BASE, "Blob");
            drop(self.create(b, BASE, "NewChild"));
            self.delete_key(b, BASE, "Child");
            for name in ["Added", "Count", "Blob", "Name"] {
                self.query(b, BASE, name, Some(64));
            }
            self.enum_keys(b, BASE, true);
            self.enum_values(b, BASE, true);
            self.info(b, BASE);
            self.open(b, BASE, "Child", KEY_READ);
            if let Some(c) = self.open(b, BASE, "Keep", KEY_READ) {
                self.info(c.0, "Base\\Keep");
            }
            // And through a handle opened after the writes.
            if let Some(again) = self.open(hk, "", &format!(r"{ROOT}\{BASE}"), KEY_READ) {
                self.enum_keys(again.0, "Base(again)", true);
                self.enum_values(again.0, "Base(again)", true);
                self.info(again.0, "Base(again)");
            }
        }
    }
}
