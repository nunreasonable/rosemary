//! Legacy IOHIDSystem parameter connections. SDL provides application input,
//! but does not expose the host's system-wide HID parameter service.
#![allow(non_snake_case)]

/// `NXEventHandle NXOpenEventStatus(void)`. A mach port name is 32 bits.
/// As when IOHIDSystem cannot be opened on Darwin, return MACH_PORT_NULL.
pub extern "C" fn NXOpenEventStatus() -> u32 {
    0
}

/// `void NXCloseEventStatus(NXEventHandle)`. No parameter connections are
/// created here, so there is no owned resource to close (including handle 0).
pub extern "C" fn NXCloseEventStatus(_handle: u32) {}
