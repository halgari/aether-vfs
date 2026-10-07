//! NVAPI probe: whether a program launched this way can use NVIDIA's NVAPI,
//! which is what DLSS and DLAA go through. It loads `nvapi64.dll` by name (the
//! loader's search order, so the prefix's `system32` copy), initialises NVAPI
//! through `nvapi_QueryInterface`, and reports one `nvapi-probe: key=value`
//! line each on stdout:
//!
//! * `nvapi64` — `loaded`, or the `GetLastError` code;
//! * `initialize` — `NvAPI_Initialize`'s status (`0` is `NVAPI_OK`);
//! * `version` — `NvAPI_GetInterfaceVersionString`;
//! * `gpus` and `gpu0` — `NvAPI_EnumPhysicalGPUs`' count and the first GPU's
//!   `NvAPI_GPU_GetFullName`;
//! * `nvngx` — whether `nvngx.dll` (the driver's NGX loader) loads;
//! * `nvidia_wine_dll_dir` — `NVIDIA_WINE_DLL_DIR` as the program sees it
//!   (`unset` when it is not), where NGX looks for the driver's other DLLs.
//!
//! Exit code: 0 when NVAPI initialised, 10 when it did not, 11 when
//! `nvapi64.dll` or its export is missing (not 2 or 3, which are
//! `vfs-injector`'s own).
#![allow(unsafe_code)]

#[cfg(not(windows))]
fn main() {
    eprintln!("vfs-fixture-nvapi is a Windows program; build it with bin/build-windows");
    std::process::exit(11);
}

#[cfg(windows)]
fn main() {
    std::process::exit(probe::run());
}

#[cfg(windows)]
mod probe {
    use std::ffi::{c_char, c_void, CStr};

    type Handle = *mut c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryA(name: *const c_char) -> Handle;
        fn GetProcAddress(module: Handle, name: *const c_char) -> *const c_void;
        fn GetLastError() -> u32;
    }

    const INIT_FAILED: i32 = 10;
    const MISSING: i32 = 11;

    // `nvapi_QueryInterface` ids, from NVIDIA's public NVAPI headers.
    const ID_INITIALIZE: u32 = 0x0150_E828;
    const ID_GET_INTERFACE_VERSION_STRING: u32 = 0x0105_3FA5;
    const ID_ENUM_PHYSICAL_GPUS: u32 = 0xE5AC_921F;
    const ID_GPU_GET_FULL_NAME: u32 = 0xCEEE_8E9F;
    const NVAPI_MAX_PHYSICAL_GPUS: usize = 64;

    type QueryInterface = unsafe extern "C" fn(u32) -> *const c_void;
    type Initialize = unsafe extern "C" fn() -> i32;
    type GetString = unsafe extern "C" fn(*mut [c_char; 64]) -> i32;
    type EnumGpus = unsafe extern "C" fn(*mut [Handle; NVAPI_MAX_PHYSICAL_GPUS], *mut u32) -> i32;
    type GpuName = unsafe extern "C" fn(Handle, *mut [c_char; 64]) -> i32;

    fn line(key: &str, value: impl std::fmt::Display) {
        println!("nvapi-probe: {key}={value}");
    }

    fn text(buf: &[c_char; 64]) -> String {
        // SAFETY: NVAPI writes a NUL-terminated string into the 64 bytes;
        // the last byte is forced to NUL in case it did not.
        let mut b = *buf;
        b[63] = 0;
        unsafe { CStr::from_ptr(b.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    pub fn run() -> i32 {
        // SAFETY: plain Win32 calls with NUL-terminated names; every function
        // pointer is checked for null before it is transmuted and called with
        // the signature NVAPI's headers give it.
        unsafe {
            line(
                "nvidia_wine_dll_dir",
                std::env::var("NVIDIA_WINE_DLL_DIR").unwrap_or_else(|_| "unset".to_string()),
            );
            let ngx = LoadLibraryA(c"nvngx.dll".as_ptr());
            if ngx.is_null() {
                line("nvngx", format!("error {}", GetLastError()));
            } else {
                line("nvngx", "loaded");
            }

            let m = LoadLibraryA(c"nvapi64.dll".as_ptr());
            if m.is_null() {
                line("nvapi64", format!("error {}", GetLastError()));
                return MISSING;
            }
            line("nvapi64", "loaded");
            let qi = GetProcAddress(m, c"nvapi_QueryInterface".as_ptr());
            if qi.is_null() {
                line("query_interface", "missing");
                return MISSING;
            }
            let qi: QueryInterface = std::mem::transmute(qi);
            let f = qi(ID_INITIALIZE);
            if f.is_null() {
                line("initialize", "missing");
                return MISSING;
            }
            let init: Initialize = std::mem::transmute(f);
            let status = init();
            line("initialize", status);
            if status != 0 {
                return INIT_FAILED;
            }

            let f = qi(ID_GET_INTERFACE_VERSION_STRING);
            if !f.is_null() {
                let get: GetString = std::mem::transmute(f);
                let mut buf = [0 as c_char; 64];
                if get(&mut buf) == 0 {
                    line("version", text(&buf));
                }
            }
            let (fe, fname) = (qi(ID_ENUM_PHYSICAL_GPUS), qi(ID_GPU_GET_FULL_NAME));
            if !fe.is_null() {
                let enumerate: EnumGpus = std::mem::transmute(fe);
                let mut gpus = [std::ptr::null_mut(); NVAPI_MAX_PHYSICAL_GPUS];
                let mut n = 0u32;
                let st = enumerate(&mut gpus, &mut n);
                line(
                    "gpus",
                    if st == 0 {
                        n.to_string()
                    } else {
                        format!("error {st}")
                    },
                );
                if st == 0 && n > 0 && !fname.is_null() {
                    let name: GpuName = std::mem::transmute(fname);
                    let mut buf = [0 as c_char; 64];
                    if name(gpus[0], &mut buf) == 0 {
                        line("gpu0", text(&buf));
                    }
                }
            }
            0
        }
    }
}
