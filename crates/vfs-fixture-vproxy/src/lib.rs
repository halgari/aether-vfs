//! Stand-in for a game's statically-imported d3d/dxgi proxy DLL.
//! The BACKING copy returns 4242 so the harness can prove the import bound
//! to our redirected image rather than any on-disk file in the app dir.

#[no_mangle]
pub extern "C" fn vproxy_value() -> i32 {
    4242
}
