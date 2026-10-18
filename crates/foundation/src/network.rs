//! Network.framework's path monitor: whether the machine is online.
//!
//! A program asks to be told about the network path, on a queue it names, and
//! decides from the answer whether to try connecting at all. The answer here is
//! always "satisfied" - the host has a network or the connections that follow
//! fail on their own, with errors the program already handles - delivered once,
//! asynchronously, as the real monitor delivers its first update.
//!
//! The objects are Objective-C objects, because with the SDK's defaults ARC
//! retains and releases `nw_*` handles with `objc_retain` and `objc_release`.
#![allow(non_snake_case)]

use std::sync::Mutex;

use objc::object::Id;

/// `nw_path_status_satisfied`.
const SATISFIED: i32 = 1;

#[derive(Default, Clone, Copy)]
struct Monitor {
    queue: u64,
    update: u64,
    cancel: u64,
    started: bool,
}

/// Each monitor's state, by address: the object is a plain instance, so that
/// `objc_release` frees it the ordinary way.
static MONITORS: Mutex<Vec<(usize, Monitor)>> = Mutex::new(Vec::new());

fn with_monitor<R>(monitor: Id, f: impl FnOnce(&mut Monitor) -> R) -> Option<R> {
    let mut monitors = MONITORS.lock().unwrap_or_else(|e| e.into_inner());
    monitors
        .iter_mut()
        .find(|(m, _)| *m == monitor as usize)
        .map(|(_, state)| f(state))
}

fn instance(name: &'static std::ffi::CStr) -> Id {
    let class = corefoundation::base::register(name, None, size_of::<usize>(), false);
    objc::object::create_instance(class, 0)
}

/// `nw_path_monitor_t nw_path_monitor_create(void)`.
unsafe extern "C" fn nw_path_monitor_create() -> Id {
    let monitor = instance(c"OS_nw_path_monitor");
    MONITORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((monitor as usize, Monitor::default()));
    monitor
}

/// `void nw_path_monitor_set_queue(nw_path_monitor_t, dispatch_queue_t)`.
unsafe extern "C" fn nw_path_monitor_set_queue(monitor: Id, queue: u64) {
    with_monitor(monitor, |m| m.queue = queue);
}

/// `void nw_path_monitor_set_update_handler(nw_path_monitor_t,
/// void (^)(nw_path_t))`.
unsafe extern "C" fn nw_path_monitor_set_update_handler(monitor: Id, handler: u64) {
    let copied = darwin::dispatch::block::copy(handler).unwrap_or(0);
    let old = with_monitor(monitor, |m| std::mem::replace(&mut m.update, copied)).unwrap_or(0);
    darwin::dispatch::block::release(old);
}

/// `void nw_path_monitor_set_cancel_handler(nw_path_monitor_t, void (^)(void))`.
unsafe extern "C" fn nw_path_monitor_set_cancel_handler(monitor: Id, handler: u64) {
    let copied = darwin::dispatch::block::copy(handler).unwrap_or(0);
    let old = with_monitor(monitor, |m| std::mem::replace(&mut m.cancel, copied)).unwrap_or(0);
    darwin::dispatch::block::release(old);
}

/// Deliver the one update, on the monitor's queue: the context is the
/// monitor, which is kept alive by the reference `start` took.
unsafe extern "C" fn deliver_update(context: u64) {
    let monitor = context as Id;
    let handler = with_monitor(monitor, |m| m.update).unwrap_or(0);
    if handler != 0 {
        let path = objc::arc::autorelease(instance(c"OS_nw_path"));
        // SAFETY: a `void (^)(nw_path_t)` block, by the Block ABI its invoke is
        // the third word and takes the block and the path.
        unsafe {
            let invoke = *((handler + 16) as *const u64);
            if invoke != 0 {
                let f: unsafe extern "C" fn(u64, Id) = std::mem::transmute(invoke);
                f(handler, path);
            }
        }
    }
    objc::arc::release(monitor);
}

unsafe extern "C" fn deliver_cancel(context: u64) {
    let monitor = context as Id;
    let handler = with_monitor(monitor, |m| m.cancel).unwrap_or(0);
    // SAFETY: a `void (^)(void)` block.
    unsafe { darwin::dispatch::block::invoke(handler) };
    objc::arc::release(monitor);
}

fn queue_of(monitor: Id) -> u64 {
    match with_monitor(monitor, |m| m.queue).unwrap_or(0) {
        0 => darwin::dispatch::queue::main_queue(),
        queue => queue,
    }
}

/// `void nw_path_monitor_start(nw_path_monitor_t)`.
unsafe extern "C" fn nw_path_monitor_start(monitor: Id) {
    let first = with_monitor(monitor, |m| !std::mem::replace(&mut m.started, true)).unwrap_or(false);
    if !first {
        return;
    }
    objc::arc::retain(monitor);
    // SAFETY: a queue the program gave, or the main queue, and a function of
    // this module's with the monitor as its context.
    unsafe { darwin::dispatch::queue::async_f(queue_of(monitor), monitor as u64, deliver_update as *const () as usize as u64) };
}

/// `void nw_path_monitor_cancel(nw_path_monitor_t)`.
unsafe extern "C" fn nw_path_monitor_cancel(monitor: Id) {
    objc::arc::retain(monitor);
    // SAFETY: as in `nw_path_monitor_start`.
    unsafe { darwin::dispatch::queue::async_f(queue_of(monitor), monitor as u64, deliver_cancel as *const () as usize as u64) };
}

/// `void nw_path_monitor_prohibit_interface_type(nw_path_monitor_t,
/// nw_interface_type_t)`: the one path reported is not tied to an interface,
/// so there is nothing to leave out of it.
unsafe extern "C" fn nw_path_monitor_prohibit_interface_type(_monitor: Id, _kind: i32) {}

/// `nw_path_status_t nw_path_get_status(nw_path_t)`.
unsafe extern "C" fn nw_path_get_status(_path: Id) -> i32 {
    SATISFIED
}

exports::exports! {
    functions {
        "nw_path_monitor_create" => nw_path_monitor_create,
        "nw_path_monitor_set_queue" => nw_path_monitor_set_queue,
        "nw_path_monitor_set_update_handler" => nw_path_monitor_set_update_handler,
        "nw_path_monitor_set_cancel_handler" => nw_path_monitor_set_cancel_handler,
        "nw_path_monitor_start" => nw_path_monitor_start,
        "nw_path_monitor_cancel" => nw_path_monitor_cancel,
        "nw_path_monitor_prohibit_interface_type" => nw_path_monitor_prohibit_interface_type,
        "nw_path_get_status" => nw_path_get_status,
    }
}
