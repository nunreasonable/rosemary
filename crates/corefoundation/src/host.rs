//! `CFHost`: resolving a name to addresses, the CFNetwork way.
//!
//! A program either resolves synchronously - `CFHostStartInfoResolution` with
//! no client, which blocks and answers - or sets a client, schedules the host on
//! a run loop and gets a callback there when the answer is in. The lookup is the
//! host's own `getaddrinfo` either way, and the addresses come back as Darwin
//! `sockaddr`s, which differ from Linux's in their first two bytes.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use crate::base::{CFIndex, CFTypeRef};
use crate::runloop::Source;

/// `CFHostInfoType`.
const ADDRESSES: i32 = 0;

/// `kCFStreamErrorDomainNetDB`: a `getaddrinfo` error code.
const DOMAIN_NETDB: i32 = 12;

/// `CFStreamError`.
#[repr(C)]
pub struct StreamError {
    domain: CFIndex,
    error: i32,
}

/// `CFHostClientCallBack`.
type ClientCallback = unsafe extern "C" fn(CFTypeRef, i32, *const StreamError, *mut c_void);

/// `CFHostClientContext`.
#[repr(C)]
pub struct ClientContext {
    version: CFIndex,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

#[derive(Default)]
struct State {
    /// Darwin `sockaddr`s, once resolved.
    addresses: Option<Vec<Vec<u8>>>,
    /// The `getaddrinfo` error of the last lookup, zero when it succeeded.
    error: i32,
    /// The array `CFHostGetAddressing` handed out, kept so that the `Get`
    /// stays valid for as long as the host does.
    array: usize,
    client: Option<(ClientCallback, usize)>,
    /// The run loops the host is scheduled on, and the source that calls the
    /// client on each.
    scheduled: Vec<(usize, usize)>,
    source: Option<Arc<Source>>,
    cancelled: bool,
}

struct Host {
    name: String,
    state: Mutex<State>,
}

#[repr(C)]
struct Body {
    isa: *const objc::class::Class,
    host: *const Host,
}

fn class() -> &'static objc::class::Class {
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let pointer = *CLASS.get_or_init(|| {
        let class = crate::base::register(c"__CFHost", None, size_of::<Body>(), true);
        class.replace_method(
            objc::sel::get("dealloc"),
            dealloc as *const c_void,
            c"v16@0:8",
        );
        objc::class::ptr(class) as usize
    });
    // SAFETY: registered above and kept for the process.
    unsafe {
        objc::class::from_ptr(pointer as *const objc::class::Class).expect("the CFHost class")
    }
}

unsafe extern "C" fn dealloc(receiver: objc::object::Id, _sel: objc::sel::Sel) {
    // SAFETY: this class's layout, whose host `CFHostCreateWithName` leaked.
    unsafe {
        let body = receiver as *mut Body;
        let host = Arc::from_raw((*body).host);
        let array = host.state.lock().unwrap_or_else(|e| e.into_inner()).array;
        if array != 0 {
            crate::api::CFRelease(array as CFTypeRef);
        }
        drop(host);
        objc::arc::finalize(receiver);
    }
}

/// # Safety
///
/// `object` is null or a live object.
unsafe fn host_of(object: CFTypeRef) -> Option<Arc<Host>> {
    // SAFETY: the caller's guarantee; the layout is checked by class.
    if object.is_null() || !unsafe { crate::base::is_kind_of(object, class()) } {
        return None;
    }
    // SAFETY: a body of this class, holding one strong reference.
    unsafe {
        let raw = (*(object as *const Body)).host;
        Arc::increment_strong_count(raw);
        Some(Arc::from_raw(raw))
    }
}

/// `CFHostRef CFHostCreateWithName(CFAllocatorRef, CFStringRef hostname)`.
///
/// # Safety
///
/// `name` is null or a live string.
pub unsafe extern "C" fn CFHostCreateWithName(_allocator: CFTypeRef, name: CFTypeRef) -> CFTypeRef {
    // SAFETY: the caller's guarantee.
    let Some(name) = (unsafe { crate::string::as_string(name) }) else {
        return std::ptr::null();
    };
    let name = String::from_utf8_lossy(&name.utf8()).into_owned();
    // SAFETY: `Body` is the registered layout.
    let body = unsafe { crate::base::allocate::<Body>(class()) };
    if body.is_null() {
        return std::ptr::null();
    }
    let host = Arc::new(Host {
        name,
        state: Mutex::new(State::default()),
    });
    // SAFETY: a fresh allocation.
    unsafe { (*body).host = Arc::into_raw(host) };
    body as CFTypeRef
}

/// A Linux `sockaddr` as Darwin lays it out: a length byte, then a one-byte
/// family with Darwin's number for it.
fn darwin_sockaddr(address: &libc::sockaddr, length: usize) -> Option<Vec<u8>> {
    // SAFETY: `address` is `length` readable bytes, per `getaddrinfo`.
    let bytes = unsafe { std::slice::from_raw_parts(address as *const _ as *const u8, length) };
    let family = match i32::from(address.sa_family) {
        libc::AF_INET => 2u8,
        libc::AF_INET6 => 30u8,
        _ => return None,
    };
    let mut out = bytes.to_vec();
    out[0] = length as u8;
    out[1] = family;
    Some(out)
}

fn resolve(name: &str) -> Result<Vec<Vec<u8>>, i32> {
    let Ok(node) = std::ffi::CString::new(name) else {
        return Err(libc::EAI_NONAME);
    };
    let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
    hints.ai_socktype = libc::SOCK_STREAM;
    let mut list: *mut libc::addrinfo = std::ptr::null_mut();
    // SAFETY: a valid name and hints, and a slot for the list.
    let status = unsafe { libc::getaddrinfo(node.as_ptr(), std::ptr::null(), &hints, &mut list) };
    if status != 0 {
        return Err(status);
    }
    let mut addresses = Vec::new();
    let mut entry = list;
    while !entry.is_null() {
        // SAFETY: a node of the list `getaddrinfo` made.
        let info = unsafe { &*entry };
        if !info.ai_addr.is_null()
            && let Some(address) =
                darwin_sockaddr(unsafe { &*info.ai_addr }, info.ai_addrlen as usize)
            && !addresses.contains(&address)
        {
            addresses.push(address);
        }
        entry = info.ai_next;
    }
    // SAFETY: the list `getaddrinfo` made.
    unsafe { libc::freeaddrinfo(list) };
    Ok(addresses)
}

fn finish(host: &Host, result: Result<Vec<Vec<u8>>, i32>) {
    let mut state = host.state.lock().unwrap_or_else(|e| e.into_inner());
    match result {
        Ok(addresses) => {
            state.addresses = Some(addresses);
            state.error = 0;
        }
        Err(error) => {
            state.addresses = None;
            state.error = error;
        }
    }
}

/// The run loop source's perform: tell the client the lookup finished.
unsafe extern "C" fn perform(info: *mut c_void) {
    // SAFETY: the host pointer `start` gave the source, kept alive by the
    // reference the source's owner holds.
    let host = unsafe { &*(info as *const Host) };
    let (client, error, cancelled) = {
        let state = host.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.client, state.error, state.cancelled)
    };
    if cancelled {
        return;
    }
    let Some((callback, context)) = client else {
        return;
    };
    let failure = StreamError {
        domain: if error == 0 {
            0
        } else {
            DOMAIN_NETDB as CFIndex
        },
        error,
    };
    let handle = HANDLES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(h, _)| *h == info as usize)
        .map(|(_, object)| *object);
    let Some(handle) = handle else {
        return;
    };
    // SAFETY: the client's callback and context, as `CFHostSetClient` gave them.
    unsafe {
        callback(
            handle as CFTypeRef,
            ADDRESSES,
            &failure,
            context as *mut c_void,
        )
    };
}

/// Which `CFHostRef` each scheduled host is, for the callback's first argument.
static HANDLES: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// `Boolean CFHostStartInfoResolution(CFHostRef, CFHostInfoType, CFStreamError *)`.
///
/// Blocking without a client; with one, the lookup runs on a thread and the
/// client hears about it on the run loops the host is scheduled on.
///
/// # Safety
///
/// `object` is null or a live object; `error` is null or writable.
pub unsafe extern "C" fn CFHostStartInfoResolution(
    object: CFTypeRef,
    info: i32,
    error: *mut StreamError,
) -> u8 {
    // SAFETY: the caller's guarantee.
    let Some(host) = (unsafe { host_of(object) }) else {
        return 0;
    };
    if info != ADDRESSES {
        // Names and reachability are not looked up here.
        return 0;
    }
    let asynchronous = {
        let mut state = host.state.lock().unwrap_or_else(|e| e.into_inner());
        state.cancelled = false;
        state.client.is_some() && !state.scheduled.is_empty()
    };
    if !asynchronous {
        let result = resolve(&host.name);
        let code = *result.as_ref().err().unwrap_or(&0);
        finish(&host, result);
        if code != 0 && !error.is_null() {
            // SAFETY: writable, per the caller.
            unsafe {
                error.write(StreamError {
                    domain: DOMAIN_NETDB as CFIndex,
                    error: code,
                })
            };
        }
        return u8::from(code == 0);
    }
    let thread_host = Arc::clone(&host);
    std::thread::spawn(move || {
        let result = resolve(&thread_host.name);
        finish(&thread_host, result);
        let state = thread_host.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(source) = &state.source {
            source.signal();
        }
        for (run_loop, _) in &state.scheduled {
            // SAFETY: a run loop the host was scheduled on, which outlives it.
            unsafe { crate::api::CFRunLoopWakeUp(*run_loop as CFTypeRef) };
        }
    });
    1
}

/// `CFArrayRef CFHostGetAddressing(CFHostRef, Boolean *hasBeenResolved)`: the
/// addresses as `CFData`s holding Darwin `sockaddr`s, owned by the host.
///
/// # Safety
///
/// `object` is null or a live object; `resolved` is null or writable.
pub unsafe extern "C" fn CFHostGetAddressing(object: CFTypeRef, resolved: *mut u8) -> CFTypeRef {
    // SAFETY: the caller's guarantee.
    let Some(host) = (unsafe { host_of(object) }) else {
        return std::ptr::null();
    };
    let mut state = host.state.lock().unwrap_or_else(|e| e.into_inner());
    if !resolved.is_null() {
        // SAFETY: writable, per the caller.
        unsafe { resolved.write(u8::from(state.addresses.is_some())) };
    }
    let Some(addresses) = state.addresses.clone() else {
        return std::ptr::null();
    };
    let items: Vec<CFTypeRef> = addresses
        .into_iter()
        .map(crate::collections::new_data)
        .collect();
    let array = crate::collections::new_array(
        &items,
        crate::callbacks::Policy::from_type_callbacks(),
        false,
    );
    for item in items {
        // SAFETY: the array took its own reference.
        unsafe { crate::api::CFRelease(item) };
    }
    if state.array != 0 {
        // SAFETY: the previous answer, which this host owned.
        unsafe { crate::api::CFRelease(state.array as CFTypeRef) };
    }
    state.array = array as usize;
    array
}

/// `Boolean CFHostSetClient(CFHostRef, CFHostClientCallBack, CFHostClientContext *)`.
///
/// FIXME (ACCURACY): the context's `retain` and `release` are not called; the
/// `info` pointer is kept as it is, which every caller here owns anyway.
///
/// # Safety
///
/// `object` is null or a live object; `context` is null or readable.
pub unsafe extern "C" fn CFHostSetClient(
    object: CFTypeRef,
    callback: Option<ClientCallback>,
    context: *const ClientContext,
) -> u8 {
    // SAFETY: the caller's guarantee.
    let Some(host) = (unsafe { host_of(object) }) else {
        return 0;
    };
    let info = match context.is_null() {
        true => 0,
        // SAFETY: readable, per the caller.
        false => unsafe { (*context).info as usize },
    };
    host.state.lock().unwrap_or_else(|e| e.into_inner()).client = callback.map(|c| (c, info));
    1
}

/// `void CFHostScheduleWithRunLoop(CFHostRef, CFRunLoopRef, CFStringRef mode)`.
///
/// # Safety
///
/// All three are null or live objects.
pub unsafe extern "C" fn CFHostScheduleWithRunLoop(
    object: CFTypeRef,
    run_loop: CFTypeRef,
    mode: CFTypeRef,
) {
    // SAFETY: the caller's guarantee.
    let Some(host) = (unsafe { host_of(object) }) else {
        return;
    };
    let mut state = host.state.lock().unwrap_or_else(|e| e.into_inner());
    let source = state
        .source
        .get_or_insert_with(|| {
            crate::runloop::new_source(Some(perform), Arc::as_ptr(&host) as usize)
        })
        .clone();
    HANDLES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((Arc::as_ptr(&host) as usize, object as usize));
    // SAFETY: a live run loop and mode, and a source this host keeps.
    unsafe { crate::api::CFRunLoopAddSource(run_loop, Arc::as_ptr(&source).cast(), mode) };
    state.scheduled.push((run_loop as usize, mode as usize));
}

/// `void CFHostUnscheduleFromRunLoop(CFHostRef, CFRunLoopRef, CFStringRef mode)`.
///
/// # Safety
///
/// As [`CFHostScheduleWithRunLoop`].
pub unsafe extern "C" fn CFHostUnscheduleFromRunLoop(
    object: CFTypeRef,
    run_loop: CFTypeRef,
    mode: CFTypeRef,
) {
    // SAFETY: the caller's guarantee.
    let Some(host) = (unsafe { host_of(object) }) else {
        return;
    };
    let mut state = host.state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(source) = &state.source {
        // SAFETY: as when it was added.
        unsafe { crate::api::CFRunLoopRemoveSource(run_loop, Arc::as_ptr(source).cast(), mode) };
    }
    state
        .scheduled
        .retain(|(l, m)| !(*l == run_loop as usize && *m == mode as usize));
    if state.scheduled.is_empty() {
        HANDLES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(h, _)| *h != Arc::as_ptr(&host) as usize);
    }
}

/// `void CFHostCancelInfoResolution(CFHostRef, CFHostInfoType)`: the client
/// is not called for a lookup still running.
///
/// # Safety
///
/// `object` is null or a live object.
pub unsafe extern "C" fn CFHostCancelInfoResolution(object: CFTypeRef, _info: i32) {
    // SAFETY: the caller's guarantee.
    if let Some(host) = unsafe { host_of(object) } {
        host.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancelled = true;
    }
}

/// `kCFStreamErrorDomainNetDB`, an `SInt32` the guest reads.
pub fn data_symbols() -> Vec<(String, u64)> {
    let name = "_kCFStreamErrorDomainNetDB";
    vec![(
        name.to_string(),
        crate::base::global_cell(name, DOMAIN_NETDB as u64),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localhost_resolves_to_darwin_addresses() {
        let addresses = resolve("localhost").expect("localhost resolves");
        assert!(!addresses.is_empty());
        for address in addresses {
            assert_eq!(address[0] as usize, address.len());
            assert!(address[1] == 2 || address[1] == 30, "family {}", address[1]);
        }
    }
}
