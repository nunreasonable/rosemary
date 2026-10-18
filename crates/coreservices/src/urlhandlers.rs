//! LaunchServices' URL scheme registration: a program telling the system it
//! opens `riotclient://` links.
//!
//! Registration on this host is a desktop entry and `xdg-mime`, which belong to
//! whatever installed the program rather than to the program itself, so the
//! calls succeed and change nothing - the answer a Mac gives when the
//! registration is already in place.
#![allow(non_snake_case)]

use std::ffi::c_void;

/// `noErr`.
const NO_ERR: i32 = 0;

/// `OSStatus LSRegisterURL(CFURLRef inURL, Boolean inUpdate)`.
pub unsafe extern "C" fn LSRegisterURL(_url: *const c_void, _update: u8) -> i32 {
    NO_ERR
}

/// `OSStatus LSSetDefaultHandlerForURLScheme(CFStringRef scheme, CFStringRef
/// bundleID)`.
pub unsafe extern "C" fn LSSetDefaultHandlerForURLScheme(_scheme: *const c_void, _bundle: *const c_void) -> i32 {
    NO_ERR
}

exports::exports! {
    functions {
        "LSRegisterURL" => LSRegisterURL,
        "LSSetDefaultHandlerForURLScheme" => LSSetDefaultHandlerForURLScheme,
    }
}
