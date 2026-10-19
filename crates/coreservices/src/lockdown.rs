//! The calls a sandboxed helper process makes to cut itself off from the
//! system services it will not need: Launch Services, the window server, the
//! Dock. Each succeeds and changes nothing, since a process here has no
//! connection to any of them that could be cut.

#![allow(non_snake_case)]

/// `void _CSCheckFixDisable(void)`: no Launch Services check-fix runs here.
pub unsafe extern "C" fn _CSCheckFixDisable() {}

/// `void _LSSetApplicationLaunchServicesServerConnectionStatus(uint64_t flags,
/// void *block)`.
pub unsafe extern "C" fn _LSSetApplicationLaunchServicesServerConnectionStatus(
    _flags: u64,
    _block: *const std::ffi::c_void,
) {
}

/// `OSStatus SetApplicationIsDaemon(Boolean)`: `noErr` - nothing shows this
/// process in a Dock to take it out of.
pub unsafe extern "C" fn SetApplicationIsDaemon(_daemon: u8) -> i32 {
    0
}

/// `CGError CGSSetDenyWindowServerConnections(bool)`: `kCGErrorSuccess`.
pub unsafe extern "C" fn CGSSetDenyWindowServerConnections(_deny: u8) -> i32 {
    0
}

exports::exports! {
    functions {
        "_CSCheckFixDisable" => _CSCheckFixDisable,
        "_LSSetApplicationLaunchServicesServerConnectionStatus" => _LSSetApplicationLaunchServicesServerConnectionStatus,
        "SetApplicationIsDaemon" => SetApplicationIsDaemon,
        "CGSSetDenyWindowServerConnections" => CGSSetDenyWindowServerConnections,
    }
}
