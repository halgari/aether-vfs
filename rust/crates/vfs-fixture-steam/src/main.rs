//! Steam API probe: what a program launched this way sees of the Steam
//! client. It loads a `steam_api64.dll` it is pointed at (the redistributable
//! every Steam game ships; nothing here links or bundles it) and reports, one
//! `steam-probe: key=value` line each on stdout:
//!
//! * `registry_pid` / `registry_pid_alive` — `HKCU\Software\Valve\Steam\
//!   ActiveProcess\pid` and whether that process exists, which is all
//!   `SteamAPI_IsSteamRunning` looks at;
//! * `is_steam_running`, `init` and how long each took;
//! * after a successful init: `app_id`, `logged_on`, `subscribed`, and how
//!   long `ISteamController::Init`, `ISteamInput::Init` and (given a manifest)
//!   `ISteamInput::SetInputActionManifestFilePath` took. The last is the call
//!   that prints `Timed out waiting for game mapping!`: it waits one second
//!   for the client to have a controller configuration loaded for the app.
//!
//! Environment: `VFS_FIXTURE_STEAM_API_DLL` is the DLL to load (default
//! `steam_api64.dll`, by the loader's search order). `VFS_FIXTURE_STEAM_OUT`
//! names a file that receives the same lines. `VFS_FIXTURE_STEAM_INPUT=0`
//! skips the controller calls; `VFS_FIXTURE_STEAM_MANIFEST` is the action
//! manifest path to set.
//!
//! Exit code: 0 when the Steam API initialised, 10 when it did not, 11 when
//! the DLL or an export is missing (not 2 or 3, which are `vfs-injector`'s
//! own).

#[cfg(not(windows))]
fn main() {
    eprintln!("vfs-fixture-steam is a Windows program; build it with bin/build-windows");
    std::process::exit(11);
}

#[cfg(windows)]
fn main() {
    std::process::exit(probe::run());
}

#[cfg(windows)]
mod probe {
    use std::ffi::{c_char, c_void, OsStr};
    use std::fmt::Write as _;
    use std::os::windows::ffi::OsStrExt;
    use std::time::Instant;

    type Handle = *mut c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryW(name: *const u16) -> Handle;
        fn GetProcAddress(module: Handle, name: *const c_char) -> *const c_void;
        fn GetLastError() -> u32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn GetExitCodeProcess(process: Handle, code: *mut u32) -> i32;
        fn CloseHandle(h: Handle) -> i32;
        fn GetCurrentProcessId() -> u32;
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn RegGetValueW(
            key: isize,
            subkey: *const u16,
            value: *const u16,
            flags: u32,
            kind: *mut u32,
            data: *mut c_void,
            len: *mut u32,
        ) -> i32;
    }

    const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as i32 as isize;
    const RRF_RT_REG_DWORD: u32 = 0x10;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    const INIT_FAILED: i32 = 10;
    const MISSING: i32 = 11;

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    struct Report(String);

    impl Report {
        fn line(&mut self, key: &str, value: impl std::fmt::Display) {
            println!("steam-probe: {key}={value}");
            let _ = writeln!(self.0, "{key}={value}");
        }

        fn finish(self, code: i32) -> i32 {
            if let Some(out) = std::env::var_os("VFS_FIXTURE_STEAM_OUT") {
                if let Err(e) = std::fs::write(&out, self.0) {
                    eprintln!("steam-probe: writing {}: {e}", out.to_string_lossy());
                }
            }
            code
        }
    }

    fn registry_pid() -> Option<u32> {
        let key = wide(OsStr::new(r"Software\Valve\Steam\ActiveProcess"));
        let value = wide(OsStr::new("pid"));
        let mut pid = 0u32;
        let mut len = 4u32;
        // SAFETY: the two strings are NUL-terminated and outlive the call;
        // `pid` is the four bytes `len` says it is.
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&mut pid as *mut u32).cast(),
                &mut len,
            )
        };
        (rc == 0).then_some(pid)
    }

    fn pid_alive(pid: u32) -> bool {
        // SAFETY: plain calls; the handle is closed before returning.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            ok && code == STILL_ACTIVE
        }
    }

    pub fn run() -> i32 {
        let mut r = Report(String::new());
        // SAFETY: no arguments.
        r.line("pid", unsafe { GetCurrentProcessId() });
        for name in [
            "SteamAppId",
            "SteamGameId",
            "SteamOverlayGameId",
            "SteamClientLaunch",
        ] {
            r.line(
                &format!("env_{name}"),
                std::env::var(name).unwrap_or_else(|_| "<unset>".into()),
            );
        }
        match registry_pid() {
            Some(pid) => {
                r.line("registry_pid", pid);
                r.line("registry_pid_alive", pid_alive(pid));
            }
            None => r.line("registry_pid", "<none>"),
        }

        let dll = std::env::var_os("VFS_FIXTURE_STEAM_API_DLL")
            .unwrap_or_else(|| "steam_api64.dll".into());
        let wdll = wide(&dll);
        // SAFETY: `wdll` is NUL-terminated and outlives the call.
        let module = unsafe { LoadLibraryW(wdll.as_ptr()) };
        if module.is_null() {
            // SAFETY: no arguments.
            let err = unsafe { GetLastError() };
            r.line(
                "load",
                format!("failed ({err}) for {}", dll.to_string_lossy()),
            );
            return r.finish(MISSING);
        }
        let sym = |name: &str| -> *const c_void {
            let c = std::ffi::CString::new(name).expect("an export name has no NUL");
            // SAFETY: `module` is a loaded module and `c` is NUL-terminated.
            unsafe { GetProcAddress(module, c.as_ptr()) }
        };
        // An export of the loaded DLL as a function of type `$ty`.
        macro_rules! export {
            ($name:literal, $ty:ty) => {{
                let p = sym($name);
                if p.is_null() {
                    r.line("missing_export", $name);
                    return r.finish(MISSING);
                }
                // SAFETY: the export has this signature in the Steamworks
                // flat API (`steam_api_flat.h`); x64 has one calling
                // convention.
                let f: $ty = unsafe { std::mem::transmute::<*const c_void, $ty>(p) };
                f
            }};
        }
        type BoolFn = unsafe extern "C" fn() -> bool;
        type VoidFn = unsafe extern "C" fn();
        type GetFn = unsafe extern "C" fn() -> *mut c_void;
        type SelfBoolFn = unsafe extern "C" fn(*mut c_void) -> bool;
        type SelfU32Fn = unsafe extern "C" fn(*mut c_void) -> u32;
        type SelfStrFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> bool;
        type InitFlatFn = unsafe extern "C" fn(*mut c_char) -> i32;
        type InputInitFn = unsafe extern "C" fn(*mut c_void, bool) -> bool;

        let is_running = export!("SteamAPI_IsSteamRunning", BoolFn);
        let shutdown = export!("SteamAPI_Shutdown", VoidFn);
        let run_callbacks = export!("SteamAPI_RunCallbacks", VoidFn);

        // SAFETY (this and every call below): the pointers are exports of the
        // loaded DLL with the signatures above, and interface pointers are
        // only used after a successful init, as the API requires.
        let t = Instant::now();
        r.line("is_steam_running", unsafe { is_running() });
        r.line("is_steam_running_ms", t.elapsed().as_millis());

        // `SteamAPI_Init` is an export up to SDK 1.57 and an inline wrapper
        // over `SteamAPI_InitFlat` after it.
        let t = Instant::now();
        let ok = if !sym("SteamAPI_Init").is_null() {
            let init = export!("SteamAPI_Init", BoolFn);
            unsafe { init() }
        } else {
            let init = export!("SteamAPI_InitFlat", InitFlatFn);
            let mut why = [0 as c_char; 1024];
            let rc = unsafe { init(why.as_mut_ptr()) };
            if rc != 0 {
                // SAFETY: the API NUL-terminates what it writes, and the
                // buffer started zeroed.
                let why = unsafe { std::ffi::CStr::from_ptr(why.as_ptr()) };
                r.line("init_error", format!("{rc}: {}", why.to_string_lossy()));
            }
            rc == 0
        };
        r.line("init", ok);
        r.line("init_ms", t.elapsed().as_millis());
        if !ok {
            return r.finish(INIT_FAILED);
        }

        // An interface accessor under whichever of `names` this SDK exports.
        let accessor = |names: &[&str]| -> Option<GetFn> {
            names
                .iter()
                .map(|n| sym(n))
                .find(|p| !p.is_null())
                .map(|p| {
                    // SAFETY: every accessor in the flat API takes nothing and
                    // returns the interface pointer.
                    unsafe { std::mem::transmute::<*const c_void, GetFn>(p) }
                })
        };
        if let Some(get) = accessor(&["SteamAPI_SteamUtils_v010", "SteamAPI_SteamUtils_v009"]) {
            let get_app_id = export!("SteamAPI_ISteamUtils_GetAppID", SelfU32Fn);
            let utils = unsafe { get() };
            if !utils.is_null() {
                r.line("app_id", unsafe { get_app_id(utils) });
            }
        }
        if let Some(get) = accessor(&[
            "SteamAPI_SteamUser_v023",
            "SteamAPI_SteamUser_v022",
            "SteamAPI_SteamUser_v021",
            "SteamAPI_SteamUser_v020",
        ]) {
            let logged_on = export!("SteamAPI_ISteamUser_BLoggedOn", SelfBoolFn);
            let user = unsafe { get() };
            if !user.is_null() {
                r.line("logged_on", unsafe { logged_on(user) });
            }
        }
        if let Some(get) = accessor(&["SteamAPI_SteamApps_v008"]) {
            let subscribed = export!("SteamAPI_ISteamApps_BIsSubscribed", SelfBoolFn);
            let apps = unsafe { get() };
            if !apps.is_null() {
                r.line("subscribed", unsafe { subscribed(apps) });
            }
        }

        if std::env::var("VFS_FIXTURE_STEAM_INPUT").as_deref() != Ok("0") {
            if let Some(get) = accessor(&["SteamAPI_SteamController_v008"]) {
                let controller_init = export!("SteamAPI_ISteamController_Init", SelfBoolFn);
                let controller = unsafe { get() };
                if !controller.is_null() {
                    let t = Instant::now();
                    r.line("controller_init", unsafe { controller_init(controller) });
                    r.line("controller_init_ms", t.elapsed().as_millis());
                }
            }
            if let Some(get) = accessor(&["SteamAPI_SteamInput_v006", "SteamAPI_SteamInput_v005"]) {
                let input_init = export!("SteamAPI_ISteamInput_Init", InputInitFn);
                let set_manifest = export!(
                    "SteamAPI_ISteamInput_SetInputActionManifestFilePath",
                    SelfStrFn
                );
                let input = unsafe { get() };
                if !input.is_null() {
                    let t = Instant::now();
                    r.line("input_init", unsafe { input_init(input, false) });
                    r.line("input_init_ms", t.elapsed().as_millis());
                    if let Ok(manifest) = std::env::var("VFS_FIXTURE_STEAM_MANIFEST") {
                        let c = std::ffi::CString::new(manifest).expect("a path has no NUL");
                        let t = Instant::now();
                        r.line("input_manifest", unsafe { set_manifest(input, c.as_ptr()) });
                        r.line("input_manifest_ms", t.elapsed().as_millis());
                    }
                }
            }
        }

        let t = Instant::now();
        for _ in 0..20 {
            unsafe { run_callbacks() };
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        r.line("callbacks_ms", t.elapsed().as_millis());
        unsafe { shutdown() };
        r.finish(0)
    }
}
