//! libresolv's `res_9_*` entry points: DNS queries a program makes itself,
//! for record types `getaddrinfo` does not cover (SRV, TXT).
//!
//! The wire format is DNS's own, the same everywhere, and `ns_msg` and `ns_rr`
//! come from BIND on both systems with the same layout - so the parsing half
//! is the host's libresolv, reached by name. The `res_state` a program
//! allocates for the `n` calls is Darwin's and is left alone: the host resolver
//! keeps its own.

use std::ffi::{c_char, c_void};
use std::sync::OnceLock;

use libc::c_int;
use std::sync::LazyLock;

unsafe extern "C" fn res_9_init() -> c_int {
    static RES_INIT_SYM: LazyLock<usize> = LazyLock::new(|| {
        let lresolv_names = [c"libresolv.so", c"libresolv.dylib"];
        for resolv in &lresolv_names {
            unsafe { libc::dlopen(resolv.as_ptr(), libc::RTLD_LAZY) };
        }
        unsafe {
            let sym = libc::dlsym(libc::RTLD_DEFAULT, c"res_init".as_ptr());
            std::mem::transmute(sym)
        }
    });
    if *RES_INIT_SYM != 0 {
        unsafe {
            let res_init: unsafe extern "C" fn() -> c_int = std::mem::transmute(*RES_INIT_SYM);
            res_init()
        }
    } else {
        // FIXME (ACCURACY): does nothing if res_init is not found
        0
    }
}

unsafe extern "C" fn res_9_close() {
    // FIXME (ACCURACY): unimplemented!
}

/// `RES_INIT`, in the `options` word at offset 8 of Darwin's `__res_state`.
const RES_INIT: u64 = 1;

unsafe extern "C" {
    fn res_query(
        dname: *const c_char,
        class: c_int,
        kind: c_int,
        answer: *mut u8,
        anslen: c_int,
    ) -> c_int;
}

type InitParse = unsafe extern "C" fn(*const u8, c_int, *mut c_void) -> c_int;
type ParseRr = unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut c_void) -> c_int;

/// The host's `ns_initparse` and `ns_parserr`, which live in libresolv rather
/// than libc.
fn parsers() -> Option<(InitParse, ParseRr)> {
    static PARSERS: OnceLock<Option<(usize, usize)>> = OnceLock::new();
    let found = *PARSERS.get_or_init(|| {
        // SAFETY: loading the system's libresolv and looking up two symbols
        // whose prototypes are the ones above.
        unsafe {
            let handle = libc::dlopen(c"libresolv.so.2".as_ptr(), libc::RTLD_NOW);
            if handle.is_null() {
                return None;
            }
            let init = libc::dlsym(handle, c"ns_initparse".as_ptr());
            let parse = libc::dlsym(handle, c"ns_parserr".as_ptr());
            (!init.is_null() && !parse.is_null()).then_some((init as usize, parse as usize))
        }
    });
    // SAFETY: the two addresses found above, with those prototypes.
    found.map(|(init, parse)| unsafe {
        (
            std::mem::transmute::<usize, InitParse>(init),
            std::mem::transmute::<usize, ParseRr>(parse),
        )
    })
}

/// `int res_9_ninit(res_state)`: marks the state initialised. The query goes
/// through the host's resolver configuration, not through this structure.
///
/// # Safety
///
/// `state` is null or a writable Darwin `__res_state`.
unsafe extern "C" fn res_ninit(state: *mut u8) -> c_int {
    if !state.is_null() {
        // SAFETY: `options` is the `u_long` at offset 8.
        unsafe { *(state.add(8) as *mut u64) |= RES_INIT };
    }
    0
}

/// `void res_9_nclose(res_state)`.
unsafe extern "C" fn res_nclose(_state: *mut u8) {}

/// `int res_9_nquery(res_state, const char *dname, int class, int type,
/// u_char *answer, int anslen)`: the answer's length, or -1.
///
/// # Safety
///
/// `name` is a C string and `answer` is `length` writable bytes.
unsafe extern "C" fn res_nquery(
    _state: *mut u8,
    name: *const c_char,
    class: c_int,
    kind: c_int,
    answer: *mut u8,
    length: c_int,
) -> c_int {
    if name.is_null() || answer.is_null() {
        return -1;
    }
    // SAFETY: the caller's guarantee.
    unsafe { res_query(name, class, kind, answer, length) }
}

/// `int res_9_ns_initparse(const u_char *msg, int msglen, ns_msg *handle)`.
///
/// # Safety
///
/// As the host's `ns_initparse`.
unsafe extern "C" fn ns_initparse(message: *const u8, length: c_int, handle: *mut c_void) -> c_int {
    match parsers() {
        // SAFETY: the caller's guarantee; the structure is the same on both.
        Some((init, _)) => unsafe { init(message, length, handle) },
        None => -1,
    }
}

/// `int res_9_ns_parserr(ns_msg *handle, ns_sect section, int rrnum, ns_rr *rr)`.
///
/// # Safety
///
/// As the host's `ns_parserr`.
unsafe extern "C" fn ns_parserr(
    handle: *mut c_void,
    section: c_int,
    index: c_int,
    record: *mut c_void,
) -> c_int {
    match parsers() {
        // SAFETY: as above.
        Some((_, parse)) => unsafe { parse(handle, section, index, record) },
        None => -1,
    }
}

/// The name servers `/etc/resolv.conf` lists, in order.
fn name_servers() -> Vec<std::net::IpAddr> {
    std::fs::read_to_string("/etc/resolv.conf")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().strip_prefix("nameserver"))
        .filter_map(|rest| rest.trim().split('%').next()?.parse().ok())
        .collect()
}

/// `int res_9_getservers(res_state, union res_sockaddr_union *, int count)`:
/// the host's name servers as Darwin `sockaddr`s on port 53, each in a
/// 128-byte slot; how many were written.
///
/// # Safety
///
/// `out` has room for `count` 128-byte slots.
unsafe extern "C" fn res_getservers(_state: *mut u8, out: *mut u8, count: c_int) -> c_int {
    const SLOT: usize = 128;
    if out.is_null() || count <= 0 {
        return 0;
    }
    let mut written = 0;
    for server in name_servers().into_iter().take(count as usize) {
        let mut slot = [0u8; SLOT];
        match server {
            std::net::IpAddr::V4(v4) => {
                slot[0] = 16;
                slot[1] = 2;
                slot[2..4].copy_from_slice(&53u16.to_be_bytes());
                slot[4..8].copy_from_slice(&v4.octets());
            }
            std::net::IpAddr::V6(v6) => {
                slot[0] = 28;
                slot[1] = 30;
                slot[2..4].copy_from_slice(&53u16.to_be_bytes());
                slot[8..24].copy_from_slice(&v6.octets());
            }
        }
        // SAFETY: slot `written` of the caller's array.
        unsafe { std::ptr::copy_nonoverlapping(slot.as_ptr(), out.add(written * SLOT), SLOT) };
        written += 1;
    }
    written as c_int
}

/// `void res_9_ndestroy(res_state)`: as `res_9_nclose`; the state holds
/// nothing of this runtime's to free.
unsafe extern "C" fn res_ndestroy(_state: *mut u8) {}

exports::exports! {
    functions {
        "res_9_init" => res_9_init,
        "res_9_close" => res_9_close,
        "res_9_ninit" => res_ninit,
        "res_9_nclose" => res_nclose,
        "res_9_ndestroy" => res_ndestroy,
        "res_9_getservers" => res_getservers,
        "res_9_nquery" => res_nquery,
        "res_9_ns_initparse" => ns_initparse,
        "res_9_ns_parserr" => ns_parserr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_parsers_are_found() {
        assert!(
            parsers().is_some(),
            "libresolv.so.2 with ns_initparse and ns_parserr"
        );
    }
}
