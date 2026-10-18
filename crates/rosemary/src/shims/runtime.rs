use std::ffi::c_char;
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};

use machish::resolve::SyntheticDylib;

/// The guest's stack canary.
static STACK_CHK_GUARD: u64 = 0x00ff_dead_beef_c0de;

/// The guest's `argc`, as `_NSGetArgc` hands out the address of.
static GUEST_ARGC: AtomicI32 = AtomicI32::new(0);

/// The guest's `argv`, as `_NSGetArgv` hands out the address of.
static GUEST_ARGV: AtomicPtr<*mut c_char> = AtomicPtr::new(std::ptr::null_mut());

/// Record the arguments the guest was entered with.
///
/// Called once the stack is laid out, which is after the chain that answers these
/// imports was built - so the two accessors below hand out the addresses of these
/// statics and read them when the guest calls, rather than at bind time.
///
/// The guest's arguments, not this process's: `argv[0]` has to be the program the
/// guest thinks it is, not `rosemary`.
pub fn set_process_args(argc: u64, argv: u64) {
    GUEST_ARGC.store(argc as i32, Ordering::Relaxed);
    GUEST_ARGV.store(argv as usize as *mut *mut c_char, Ordering::Relaxed);
    // And the process's name, which the host's `warn` and `err` print.
    if argc > 0 && argv != 0 {
        // SAFETY: the vector just laid out on the guest's stack, whose first
        // entry is a NUL-terminated string in this address space.
        let argv0 = unsafe { *(argv as usize as *const *const c_char) };
        if !argv0.is_null() {
            darwin::progname::set(unsafe { std::ffi::CStr::from_ptr(argv0) });
        }
    }
}

/// `int *_NSGetArgc(void)`.
extern "C" fn ns_get_argc() -> *mut i32 {
    GUEST_ARGC.as_ptr()
}

/// `char ***_NSGetArgv(void)`.
extern "C" fn ns_get_argv() -> *mut *mut *mut c_char {
    GUEST_ARGV.as_ptr()
}

/// The guest's own executable, as `_NSGetExecutablePath` must report it.
///
/// Empty until [`set_executable_path`] runs, which is before any guest code.
static GUEST_EXECUTABLE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Record the program the guest is, for the reason [`set_process_args`] records
/// its arguments: the path has to be the one the guest thinks it is running
/// from, not this runtime's.
///
/// Absolute, because a caller joins onto it, but with its symlinks kept, as
/// `Layout::enclosing` keeps them.
pub fn set_executable_path(path: &std::path::Path) {
    let resolved = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    // What the guest is told, so the *guest* spelling: a program outside the
    // prefix is named through the `/Volumes/Host` mount rather than by a host
    // path it would go on to compare and store. See `docs/prefixes.md`.
    let spelled = prefix::ns::to_guest(&resolved).unwrap_or(resolved);
    *GUEST_EXECUTABLE.lock().unwrap_or_else(|e| e.into_inner()) =
        spelled.to_string_lossy().into_owned();
}

/// The environment slot used by the selected guest libc on every host.
fn guest_environ() -> Option<u64> {
    Some(darwin::env::guest_environ_slot() as usize as u64)
}

/// `char ***_NSGetEnviron(void)`.
extern "C" fn ns_get_environ() -> *mut *mut *mut c_char {
    darwin::env::guest_environ_slot()
}

/// Environment symbols must agree for imports and libSystem `dlsym` calls.
pub fn host_substitute(name: &str) -> Option<u64> {
    match name {
        "environ" => guest_environ(),
        "_NSGetEnviron" => Some(ns_get_environ as *const () as usize as u64),
        _ => None,
    }
}

/// `int _NSGetExecutablePath(char *buf, uint32_t *bufsize)`.
///
/// # Safety
///
/// Called only through an import slot this module defines, with the two arguments the
/// declaration gives it.
unsafe extern "C" fn ns_get_executable_path(buf: *mut u8, bufsize: *mut u32) -> i32 {
    if bufsize.is_null() {
        return -1;
    }
    // The guest's, not this process's. Only a runtime that was handed no path -
    // which is no run that reaches a guest - falls back to its own.
    let path = GUEST_EXECUTABLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let path = match path.is_empty() {
        false => path,
        true => std::env::current_exe()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    };
    let needed = path.len() + 1;
    // SAFETY: a guest `uint32_t *`, directly addressable under the identity mapping.
    let available = unsafe { *bufsize } as usize;
    if buf.is_null() || available < needed {
        unsafe { *bufsize = needed as u32 };
        return -1;
    }
    // SAFETY: `buf` has at least `needed` bytes, just checked against the caller's count.
    unsafe {
        std::ptr::copy_nonoverlapping(path.as_ptr(), buf, path.len());
        *buf.add(path.len()) = 0;
    }
    0
}

/// `h_errno`: the resolver's error, a plain `int` in libSystem. Nothing here
/// sets it - the resolver calls a guest makes answer through their return
/// values - but a program that reads it after a failed lookup reads a zero
/// rather than faulting on an unbound slot.
static H_ERRNO: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Symbols the runtime supplies itself, consulted before the host.
pub fn provider() -> SyntheticDylib {
    let mut lib = SyntheticDylib::new("<rosemary runtime>");
    lib.flat(true).define_data(
        "___stack_chk_guard",
        &STACK_CHK_GUARD as *const u64 as usize as u64,
    );
    lib.define_data("_h_errno", H_ERRNO.as_ptr() as usize as u64);
    // Reached only through a descriptor `tls::register` declined; dyld's TLV ABI
    // preserves every register except the return value.
    for name in machish::extensions::tlv::BOOTSTRAP_SYMBOLS {
        lib.define_fn(name, crate::tls::tlv_bootstrap_thunk as *const ());
    }
    // This runtime's own on every host, so `thread_local` destructors run in
    // Darwin's order. See [`crate::tls`].
    lib.define_fn("__tlv_atexit", crate::tls::tlv_atexit as *const ());
    lib.define_fn("_tlv_atexit", crate::tls::tlv_atexit as *const ());
    // libSystem's `exit` runs those terminators before any `atexit` handler.
    lib.define_fn("_exit", crate::tls::darwin_exit as *const ());
    super::openat::provide(&mut lib);
    lib.define_fn("__NSGetExecutablePath", ns_get_executable_path as *const ());
    lib.define_fn(
        "__dyld_find_unwind_sections",
        crate::dyld::entry::dyld_find_unwind_sections as *const (),
    );
    if !darwin::libc_backend::using_builtin() {
        if let Some(port) = crate::host::symbol("mach_task_self_") {
            lib.define_data("_mach_task_self_", port);
        }
        if let Some(f) = crate::host::symbol("mach_host_self") {
            lib.define_fn("_mach_host_self", f as *const ());
        }
    }
    lib.define_fn("__NSGetArgc", ns_get_argc as *const ());
    lib.define_fn("__NSGetArgv", ns_get_argv as *const ());
    if let Some(addr) = guest_environ() {
        lib.define_data("_environ", addr);
        lib.define_fn("__NSGetEnviron", ns_get_environ as *const ());
    }
    lib
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_has_a_zero_byte() {
        assert!(
            STACK_CHK_GUARD.to_le_bytes().contains(&0),
            "a canary without a zero byte can be copied back by a string overflow"
        );
    }

    #[test]
    fn the_executable_path_round_trips_and_reports_the_size_it_needs() {
        let expected = std::env::current_exe().unwrap();
        let expected = expected.to_string_lossy();

        // Too small a buffer must change nothing and report the size required,
        // including the NUL.
        let mut size: u32 = 1;
        // SAFETY: a null buffer with a size the function is being asked to grow.
        let rc = unsafe { ns_get_executable_path(std::ptr::null_mut(), &mut size) };
        assert_eq!(rc, -1);
        assert_eq!(size as usize, expected.len() + 1);

        let mut buf = vec![0xAAu8; size as usize + 8];
        // SAFETY: `buf` is at least `size` bytes, which is what was just asked for.
        let rc = unsafe { ns_get_executable_path(buf.as_mut_ptr(), &mut size) };
        assert_eq!(rc, 0);
        let end = buf.iter().position(|b| *b == 0).expect("NUL-terminated");
        assert_eq!(&buf[..end], expected.as_bytes());
        assert!(buf[end + 1..].iter().all(|b| *b == 0xAA));
    }
}
