//! Accessibility trust and event taps: the two halves of reading another
//! application's input.
//!
//! On a Mac both need the user to grant the program "Accessibility" or "Input
//! Monitoring" in System Settings. Nothing here grants either, so the answers
//! are the ones a Mac gives a program the user has not allowed: not trusted,
//! and an event tap that cannot be created. A program checks for exactly that
//! and carries on without the feature.
#![allow(non_snake_case)]

use std::ffi::c_void;

/// `Boolean AXIsProcessTrusted(void)`.
pub unsafe extern "C" fn AXIsProcessTrusted() -> u8 {
    0
}

/// `Boolean AXIsProcessTrustedWithOptions(CFDictionaryRef)`. Asking for the
/// prompt shows none: there is no System Settings to send the user to.
pub unsafe extern "C" fn AXIsProcessTrustedWithOptions(_options: *const c_void) -> u8 {
    0
}

/// `CFMachPortRef CGEventTapCreate(CGEventTapLocation, CGEventTapPlacement,
/// CGEventTapOptions, CGEventMask, CGEventTapCallBack, void *)`: null, which is
/// what a process without the permission gets.
pub unsafe extern "C" fn CGEventTapCreate(
    _tap: u32,
    _place: u32,
    _options: u32,
    _mask: u64,
    _callback: *const c_void,
    _info: *mut c_void,
) -> *const c_void {
    std::ptr::null()
}

/// `void CGEventTapEnable(CFMachPortRef, bool)`: there is never a tap to enable.
pub unsafe extern "C" fn CGEventTapEnable(_tap: *const c_void, _enable: u8) {}

/// `int64_t CGEventGetIntegerValueField(CGEventRef, CGEventField)`: only
/// reached with an event a tap delivered, which none does.
pub unsafe extern "C" fn CGEventGetIntegerValueField(_event: *const c_void, _field: u32) -> i64 {
    0
}
