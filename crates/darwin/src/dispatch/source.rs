//! Dispatch sources: a handler run on a queue when something happens.
//!
//! Each source that has been resumed has a thread of its own watching for its
//! event - a deadline, a readable descriptor, a Mach message, a process exit -
//! and hands the event handler to the source's queue. The watcher does not
//! look again until the handler has run, which is libdispatch's guarantee
//! that a source's handler never runs concurrently with itself, and what makes
//! a level-triggered event (a message still queued) fire once per handling
//! rather than once per look.
//!
//! Kinds with nothing behind them here - memory pressure, a vnode, a signal,
//! a Mach send right's death - are accepted and never fire, which is what each
//! of them does on a Mac where the event does not happen.

use std::ffi::c_void;
use std::sync::{Condvar, Mutex};

use super::block;
use super::object::{self, Header, Kind};
use super::time;

/// The `dispatch_source_type_t`s, each the address of its own exported word.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Type {
    DataAdd,
    DataOr,
    DataReplace,
    MachSend,
    MachRecv,
    MemoryPressure,
    Proc,
    Read,
    Signal,
    Timer,
    Vnode,
    Write,
}

const TYPES: [(Type, &str); 12] = [
    (Type::DataAdd, "__dispatch_source_type_data_add"),
    (Type::DataOr, "__dispatch_source_type_data_or"),
    (Type::DataReplace, "__dispatch_source_type_data_replace"),
    (Type::MachSend, "__dispatch_source_type_mach_send"),
    (Type::MachRecv, "__dispatch_source_type_mach_recv"),
    (Type::MemoryPressure, "__dispatch_source_type_memorypressure"),
    (Type::Proc, "__dispatch_source_type_proc"),
    (Type::Read, "__dispatch_source_type_read"),
    (Type::Signal, "__dispatch_source_type_signal"),
    (Type::Timer, "__dispatch_source_type_timer"),
    (Type::Vnode, "__dispatch_source_type_vnode"),
    (Type::Write, "__dispatch_source_type_write"),
];

/// The words the type symbols name: their addresses are the types.
static TYPE_WORDS: [u64; 12] = [0; 12];

/// The address a type symbol names, for the exports table.
pub fn type_address(symbol: &str) -> u64 {
    let i = TYPES
        .iter()
        .position(|(_, name)| *name == symbol)
        .expect("a source type this module defines");
    &TYPE_WORDS[i] as *const u64 as u64
}

fn type_of(address: u64) -> Option<Type> {
    TYPES
        .iter()
        .enumerate()
        .find(|(i, _)| &TYPE_WORDS[*i] as *const u64 as u64 == address)
        .map(|(_, (t, _))| *t)
}

/// `NOTE_EXIT`, the process event a `proc` source reports.
const NOTE_EXIT: u64 = 0x8000_0000;

/// A handler: a copied block, or a function called with the source's context.
#[derive(Clone, Copy, Default)]
enum Handler {
    #[default]
    None,
    Block(u64),
    Function(u64),
}

impl Handler {
    fn release(self) {
        if let Handler::Block(b) = self {
            block::release(b);
        }
    }
}

#[derive(Default)]
struct State {
    event: Handler,
    cancel: Handler,
    registration: Handler,
    /// Darwin's suspend count; a source is created with one.
    suspended: u32,
    cancelled: bool,
    /// The cancel handler has been handed to the queue.
    cancel_sent: bool,
    /// The event handler is on the queue or running.
    in_flight: bool,
    watching: bool,
    /// Accumulated since the handler last ran.
    pending: u64,
    /// What `dispatch_source_get_data` reports, inside the handler.
    data: u64,
    /// Timer: the next deadline, the interval, and whether it was set at all.
    deadline: u64,
    interval: u64,
    armed: bool,
}

#[repr(C)]
pub struct Source {
    #[allow(dead_code)]
    header: Header,
    kind: Type,
    handle: u64,
    mask: u64,
    queue: std::sync::atomic::AtomicU64,
    state: Mutex<State>,
    changed: Condvar,
}

/// # Safety
///
/// `handle` is null or a live dispatch object.
unsafe fn source<'a>(handle: u64) -> Option<&'a Source> {
    // SAFETY: the caller's guarantee; a `Source` begins with its header.
    unsafe { object::typed(handle, Kind::Source) }.map(|_| unsafe { &*(handle as *const Source) })
}

/// `dispatch_source_t dispatch_source_create(dispatch_source_type_t, uintptr_t
/// handle, uintptr_t mask, dispatch_queue_t)`: suspended, as Darwin's is.
///
/// # Safety
///
/// `queue` is null or a live queue.
pub unsafe extern "C" fn source_create(kind: u64, handle: u64, mask: u64, queue: u64) -> u64 {
    let Some(kind) = type_of(kind) else {
        return 0;
    };
    if trace() {
        eprintln!("rosemary: dispatch source: {kind:?} on {handle:#x} mask {mask:#x}");
    }
    let queue = match queue {
        0 => unsafe { super::queue::get_global_queue(0, 0) },
        q => q,
    };
    object::retain(queue);
    Box::into_raw(Box::new(Source {
        header: Header::new(Kind::Source),
        kind,
        handle,
        mask,
        queue: std::sync::atomic::AtomicU64::new(queue),
        state: Mutex::new(State {
            suspended: 1,
            deadline: time::FOREVER,
            interval: time::FOREVER,
            ..State::default()
        }),
        changed: Condvar::new(),
    })) as u64
}

/// `dispatch_set_target_queue` on a source: its handlers run on `queue` from
/// now on. Whether `handle` was a source.
pub(super) fn set_queue(handle: u64, queue: u64) -> bool {
    // SAFETY: a live dispatch object, by the caller of the entry point.
    let Some(source) = (unsafe { source(handle) }) else {
        return false;
    };
    let queue = match queue {
        0 => unsafe { super::queue::get_global_queue(0, 0) },
        q => q,
    };
    object::retain(queue);
    let old = source.queue.swap(queue, std::sync::atomic::Ordering::AcqRel);
    // SAFETY: the reference the source held on its old queue.
    unsafe { object::release(old) };
    true
}

/// Free what a source holds once its last reference goes.
pub(super) fn finalize(source: &Source) {
    let state = source.state.lock().unwrap_or_else(|e| e.into_inner());
    state.event.release();
    state.cancel.release();
    state.registration.release();
    drop(state);
    // SAFETY: the reference `source_create` took.
    unsafe { object::release(source.queue.load(std::sync::atomic::Ordering::Acquire)) };
}

fn update(handle: u64, f: impl FnOnce(&mut State)) {
    // SAFETY: a live source, by the caller of the entry point.
    let Some(source) = (unsafe { source(handle) }) else {
        return;
    };
    let mut state = source.state.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut state);
    drop(state);
    source.changed.notify_all();
}

fn set_handler(handle: u64, pick: fn(&mut State) -> &mut Handler, handler: Handler) {
    let mut old = Handler::None;
    update(handle, |state| old = std::mem::replace(pick(state), handler));
    old.release();
}

fn block_handler(block: u64) -> Handler {
    match block::copy(block) {
        Some(b) if b != 0 => Handler::Block(b),
        _ => Handler::None,
    }
}

fn function_handler(function: u64) -> Handler {
    match function {
        0 => Handler::None,
        f => Handler::Function(f),
    }
}

/// `void dispatch_source_set_event_handler(dispatch_source_t, dispatch_block_t)`.
pub unsafe extern "C" fn set_event_handler(handle: u64, handler: u64) {
    set_handler(handle, |s| &mut s.event, block_handler(handler));
}

/// `void dispatch_source_set_event_handler_f(dispatch_source_t, dispatch_function_t)`.
pub unsafe extern "C" fn set_event_handler_f(handle: u64, handler: u64) {
    set_handler(handle, |s| &mut s.event, function_handler(handler));
}

/// `void dispatch_source_set_cancel_handler(dispatch_source_t, dispatch_block_t)`.
pub unsafe extern "C" fn set_cancel_handler(handle: u64, handler: u64) {
    set_handler(handle, |s| &mut s.cancel, block_handler(handler));
}

/// `void dispatch_source_set_cancel_handler_f(dispatch_source_t, dispatch_function_t)`.
pub unsafe extern "C" fn set_cancel_handler_f(handle: u64, handler: u64) {
    set_handler(handle, |s| &mut s.cancel, function_handler(handler));
}

/// `void dispatch_source_set_registration_handler(dispatch_source_t, dispatch_block_t)`.
pub unsafe extern "C" fn set_registration_handler(handle: u64, handler: u64) {
    set_handler(handle, |s| &mut s.registration, block_handler(handler));
}

/// `void dispatch_source_set_registration_handler_f(dispatch_source_t, dispatch_function_t)`.
pub unsafe extern "C" fn set_registration_handler_f(handle: u64, handler: u64) {
    set_handler(handle, |s| &mut s.registration, function_handler(handler));
}

/// `void dispatch_source_set_timer(dispatch_source_t, dispatch_time_t start,
/// uint64_t interval, uint64_t leeway)`. The leeway is not used: every timer
/// here fires as close to its deadline as the host lets it.
pub unsafe extern "C" fn set_timer(handle: u64, start: u64, interval: u64, _leeway: u64) {
    let start = match start {
        time::NOW => time::now(),
        s => s,
    };
    update(handle, |state| {
        state.deadline = start;
        state.interval = match interval {
            0 => 1,
            i => i,
        };
        state.armed = true;
        state.pending = 0;
    });
}

/// `void dispatch_source_merge_data(dispatch_source_t, uintptr_t)`.
pub unsafe extern "C" fn merge_data(handle: u64, value: u64) {
    // SAFETY: a live source.
    let Some(kind) = (unsafe { source(handle) }).map(|s| s.kind) else {
        return;
    };
    update(handle, |state| {
        state.pending = match kind {
            Type::DataOr => state.pending | value,
            Type::DataReplace => value,
            _ => state.pending.wrapping_add(value),
        };
    });
}

/// `uintptr_t dispatch_source_get_data(dispatch_source_t)`.
pub unsafe extern "C" fn get_data(handle: u64) -> u64 {
    // SAFETY: a live source.
    unsafe { source(handle) }.map_or(0, |s| s.state.lock().unwrap_or_else(|e| e.into_inner()).data)
}

/// `uintptr_t dispatch_source_get_handle(dispatch_source_t)`.
pub unsafe extern "C" fn get_handle(handle: u64) -> u64 {
    // SAFETY: a live source.
    unsafe { source(handle) }.map_or(0, |s| s.handle)
}

/// `uintptr_t dispatch_source_get_mask(dispatch_source_t)`.
pub unsafe extern "C" fn get_mask(handle: u64) -> u64 {
    // SAFETY: a live source.
    unsafe { source(handle) }.map_or(0, |s| s.mask)
}

/// `void dispatch_source_cancel(dispatch_source_t)`: no further events; the
/// cancel handler runs once the handler in flight, if any, has returned.
pub unsafe extern "C" fn cancel(handle: u64) {
    update(handle, |state| state.cancelled = true);
    // SAFETY: a live source.
    if let Some(source) = unsafe { source(handle) } {
        let mut state = source.state.lock().unwrap_or_else(|e| e.into_inner());
        // A source never resumed has no watcher to send the cancel handler.
        if !state.watching {
            send_cancel(handle, &mut state);
        }
    }
}

/// `intptr_t dispatch_source_testcancel(dispatch_source_t)`.
pub unsafe extern "C" fn testcancel(handle: u64) -> i64 {
    // SAFETY: a live source.
    unsafe { source(handle) }.map_or(0, |s| {
        i64::from(s.state.lock().unwrap_or_else(|e| e.into_inner()).cancelled)
    })
}

/// Resume a source; the first resume that leaves it unsuspended starts its
/// watcher. Whether `handle` was a source.
pub(super) fn resume(handle: u64) -> bool {
    // SAFETY: a live dispatch object, by the caller of `dispatch_resume`.
    let Some(source) = (unsafe { source(handle) }) else {
        return false;
    };
    let mut state = source.state.lock().unwrap_or_else(|e| e.into_inner());
    state.suspended = state.suspended.saturating_sub(1);
    let start = state.suspended == 0 && !state.watching && !state.cancelled;
    if start {
        state.watching = true;
        if let registration @ (Handler::Block(_) | Handler::Function(_)) = state.registration {
            state.registration = Handler::None;
            object::retain(handle);
            let work = Box::into_raw(Box::new((handle, registration))) as u64;
            // SAFETY: the source's queue, and a function of this module's.
            unsafe { super::queue::async_f(source.queue.load(std::sync::atomic::Ordering::Acquire), work, run_once as *const () as u64) };
        }
    }
    drop(state);
    source.changed.notify_all();
    if start {
        object::retain(handle);
        let spawned = std::thread::Builder::new()
            .name("dispatch-source".into())
            .stack_size(256 * 1024)
            .spawn(move || watch(handle));
        if spawned.is_err() {
            // SAFETY: the reference taken for the watcher that did not start.
            unsafe { object::release(handle) };
        }
    }
    true
}

/// Suspend a source. Whether `handle` was a source.
pub(super) fn suspend(handle: u64) -> bool {
    // SAFETY: a live dispatch object.
    if unsafe { source(handle) }.is_none() {
        return false;
    }
    update(handle, |state| state.suspended += 1);
    true
}

/// Run a registration or cancel handler: the work is a boxed source and
/// handler, and the source's reference is released after.
unsafe extern "C" fn run_once(work: *mut c_void) {
    // SAFETY: boxed by whoever queued this.
    let (handle, handler) = *unsafe { Box::from_raw(work as *mut (u64, Handler)) };
    // SAFETY: the reference taken when this was queued keeps the source alive.
    unsafe { call(handle, handler) };
    handler.release();
    // SAFETY: that reference.
    unsafe { object::release(handle) };
}

/// # Safety
///
/// `handle` is a live source and `handler` one of its own.
unsafe fn call(handle: u64, handler: Handler) {
    match handler {
        Handler::None => {}
        // SAFETY: a copied block, alive while the source holds it.
        Handler::Block(b) => unsafe { block::invoke(b) },
        Handler::Function(f) => {
            // SAFETY: a `dispatch_function_t` the guest gave, with the
            // source's context, as libdispatch calls it.
            let context = unsafe { object::get_context(handle) };
            let f: unsafe extern "C" fn(*mut c_void) = unsafe { std::mem::transmute(f) };
            unsafe { f(context) };
        }
    }
}

/// `ROSEMARY_TRACE_DISPATCH_SOURCE=1`: say when a source is made and fires.
fn trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ROSEMARY_TRACE_DISPATCH_SOURCE").is_some())
}

/// Run the event handler on the source's queue, with the data accumulated.
unsafe extern "C" fn fire(context: *mut c_void) {
    let handle = context as u64;
    if trace() {
        // SAFETY: the watcher's reference keeps it alive.
        if let Some(source) = unsafe { source(handle) } {
            eprintln!("rosemary: dispatch source: {:?} on {:#x} fires", source.kind, source.handle);
        }
    }
    // SAFETY: the reference the watcher took keeps it alive.
    let Some(source) = (unsafe { source(handle) }) else {
        return;
    };
    let handler = {
        let mut state = source.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.cancelled {
            Handler::None
        } else {
            state.data = std::mem::take(&mut state.pending);
            state.event
        }
    };
    // SAFETY: the source's own handler. Replacing it while it runs is a race
    // on Darwin too, and the caller's to avoid.
    unsafe { call(handle, handler) };
    let mut state = source.state.lock().unwrap_or_else(|e| e.into_inner());
    state.in_flight = false;
    drop(state);
    source.changed.notify_all();
}

fn send_cancel(handle: u64, state: &mut State) {
    if state.cancel_sent {
        return;
    }
    state.cancel_sent = true;
    let handler = std::mem::take(&mut state.cancel);
    if matches!(handler, Handler::None) {
        return;
    }
    // SAFETY: a live source.
    let Some(source) = (unsafe { source(handle) }) else {
        return;
    };
    object::retain(handle);
    let work = Box::into_raw(Box::new((handle, handler))) as u64;
    // SAFETY: the source's queue, and a function of this module's.
    unsafe { super::queue::async_f(source.queue.load(std::sync::atomic::Ordering::Acquire), work, run_once as *const () as u64) };
}

/// How long the watcher sleeps between looks at a descriptor, so that a
/// cancel or a suspend is noticed without a descriptor of its own to wake it.
const POLL_MS: i32 = 50;

/// Whether `fd` is ready for `events` within `ms`.
fn poll_fd(fd: i32, events: i16, ms: i32) -> bool {
    let mut p = libc::pollfd { fd, events, revents: 0 };
    // SAFETY: one `pollfd`, on the stack.
    unsafe { libc::poll(&mut p, 1, ms) > 0 && p.revents != 0 }
}

/// What a level-triggered kind sees right now: the data to report, or `None`.
fn look(source: &Source, ms: i32) -> Option<u64> {
    match source.kind {
        Type::Read => {
            let fd = source.handle as i32;
            poll_fd(fd, libc::POLLIN, ms).then(|| {
                let mut available: libc::c_int = 0;
                // SAFETY: `FIONREAD` writes one int.
                unsafe { libc::ioctl(fd, libc::FIONREAD, &mut available) };
                available.max(1) as u64
            })
        }
        Type::Write => poll_fd(source.handle as i32, libc::POLLOUT, ms).then_some(1),
        Type::MachRecv => {
            let port = crate::mach::space::receiver(source.handle as u32)?;
            match port.ready() || poll_fd(port.fd(), libc::POLLIN, ms) {
                true => Some(1),
                false => None,
            }
        }
        Type::Proc => {
            if source.mask & NOTE_EXIT == 0 {
                std::thread::sleep(std::time::Duration::from_millis(ms as u64));
                return None;
            }
            let pid = source.handle as i32;
            // SAFETY: probing for existence only.
            let alive = unsafe { libc::kill(pid, 0) } == 0
                || crate::errno::get() != libc::ESRCH;
            match alive {
                true => {
                    std::thread::sleep(std::time::Duration::from_millis(ms as u64));
                    None
                }
                false => Some(NOTE_EXIT),
            }
        }
        _ => None,
    }
}

/// The watcher: one per resumed source, until it is cancelled.
fn watch(handle: u64) {
    // SAFETY: the reference `resume` took for this thread.
    let source = unsafe { source(handle) }.expect("a source");
    let mut fired_proc = false;
    loop {
        let mut state = source.state.lock().unwrap_or_else(|e| e.into_inner());
        // Wait while there is nothing to do: suspended, or the last handler
        // still running.
        while !state.cancelled && (state.suspended > 0 || state.in_flight) {
            state = source.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        if state.cancelled {
            if !state.in_flight {
                send_cancel(handle, &mut state);
                break;
            }
            state = source.changed.wait(state).unwrap_or_else(|e| e.into_inner());
            drop(state);
            continue;
        }
        let ready = match source.kind {
            Type::Timer => {
                if !state.armed || state.deadline == time::FOREVER {
                    drop(source.changed.wait(state).unwrap_or_else(|e| e.into_inner()));
                    continue;
                }
                match time::remaining(state.deadline) {
                    Some(0) => {
                        let late = time::now().saturating_sub(state.deadline);
                        let missed = match state.interval {
                            time::FOREVER => 0,
                            i => late / i,
                        };
                        state.pending += 1 + missed;
                        state.deadline = match state.interval {
                            time::FOREVER => time::FOREVER,
                            i => state.deadline.saturating_add(i * (missed + 1)),
                        };
                        true
                    }
                    Some(ns) => {
                        let wait = std::time::Duration::from_nanos(ns);
                        drop(source.changed.wait_timeout(state, wait));
                        continue;
                    }
                    None => false,
                }
            }
            Type::DataAdd | Type::DataOr | Type::DataReplace => {
                if state.pending == 0 {
                    drop(source.changed.wait(state).unwrap_or_else(|e| e.into_inner()));
                    continue;
                }
                true
            }
            Type::Read | Type::Write | Type::MachRecv | Type::Proc => {
                if fired_proc {
                    drop(source.changed.wait(state).unwrap_or_else(|e| e.into_inner()));
                    continue;
                }
                drop(state);
                let seen = look(source, POLL_MS);
                state = source.state.lock().unwrap_or_else(|e| e.into_inner());
                match seen {
                    Some(data) if !state.cancelled && state.suspended == 0 => {
                        fired_proc = source.kind == Type::Proc;
                        match source.kind {
                            Type::Proc => state.pending |= data,
                            _ => state.pending = data,
                        }
                        true
                    }
                    _ => false,
                }
            }
            // Nothing here produces these events.
            _ => {
                drop(source.changed.wait(state).unwrap_or_else(|e| e.into_inner()));
                continue;
            }
        };
        if ready {
            state.in_flight = true;
            drop(state);
            // SAFETY: the source's queue; the watcher's reference keeps the
            // source alive until the handler is done, since it waits for it.
            unsafe { super::queue::async_f(source.queue.load(std::sync::atomic::Ordering::Acquire), handle, fire as *const () as u64) };
        }
    }
    // SAFETY: the reference `resume` took for this thread.
    unsafe { object::release(handle) };
}
