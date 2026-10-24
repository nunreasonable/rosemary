//! Dispatch sources. Only the timer type exists: the others watch descriptors,
//! processes, signals and Mach ports, and each needs its own kernel plumbing.
//!
//! The rules followed are libdispatch's: a source starts inactive and does
//! nothing until resumed or activated; events that arrive while it is
//! suspended or while its handler runs are coalesced into one delivery whose
//! `dispatch_source_get_data` is how many there were; cancellation stops
//! further deliveries but not one already running, and the cancel handler runs
//! once, after it, on the target queue.

use std::ffi::c_void;
use std::sync::Mutex;

use super::block;
use super::object::{self, Function, Header, Kind};
use super::queue;
use super::time;

/// `struct dispatch_source_type_s`. A guest only ever takes the address of one.
#[repr(C)]
pub struct SourceType {
    _tag: u64,
}

/// `_dispatch_source_type_timer`, which `DISPATCH_SOURCE_TYPE_TIMER` names.
pub static TYPE_TIMER: SourceType = SourceType { _tag: 1 };

/// A handler as the guest installed it.
#[derive(Clone, Copy)]
enum Handler {
    /// A block this module copied and owns one reference to.
    Block(u64),
    /// A `dispatch_function_t`, called with the source's context.
    Function(u64),
}

impl Handler {
    fn release(self) {
        if let Handler::Block(b) = self {
            block::release(b);
        }
    }
}

/// When a timer next fires, and how often after that.
struct Timer {
    /// Absolute `dispatch_time_t`; [`time::FOREVER`] once it never fires again.
    target: u64,
    /// `None` for a one-shot timer.
    interval: Option<u64>,
}

struct State {
    /// Created inactive, until the first resume or an activate.
    inactive: bool,
    suspended: u32,
    cancelled: bool,
    /// The cancel handler has been submitted, or there was none to submit.
    cancel_submitted: bool,
    event: Option<Handler>,
    cancel: Option<Handler>,
    timer: Option<Timer>,
    /// A deadline is with the timer thread for the current generation.
    armed: bool,
    /// Bumped by every reset and by cancellation, so a deadline armed before
    /// either is recognised as stale when it comes due.
    generation: u64,
    /// Events not yet delivered.
    pending: u64,
    /// What `dispatch_source_get_data` answers: the count the running, or the
    /// last, delivery was given.
    data: u64,
    /// A delivery is queued on, or running on, the target queue.
    delivering: bool,
}

impl State {
    fn running(&self) -> bool {
        !self.inactive && self.suspended == 0
    }
}

/// `#[repr(C)]` with the header first, for the reason `Queue` gives.
#[repr(C)]
pub struct Source {
    #[allow(dead_code)]
    header: Header,
    handle: u64,
    mask: u64,
    target: u64,
    state: Mutex<State>,
}

impl Drop for Source {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(|e| e.into_inner());
        for handler in [state.event.take(), state.cancel.take()]
            .into_iter()
            .flatten()
        {
            handler.release();
        }
        // SAFETY: the reference `create` took.
        unsafe { object::release(self.target) };
    }
}

/// The source a handle names, if it names a live one.
///
/// # Safety
///
/// `handle` is null or an object this crate allocated.
unsafe fn source<'a>(handle: u64) -> Option<&'a Source> {
    // SAFETY: the caller's guarantee, and the kind check makes the cast sound.
    unsafe { object::typed(handle, Kind::Source) }
        .map(|h| unsafe { &*(h as *const Header as *const Source) })
}

fn lock(source: &Source) -> std::sync::MutexGuard<'_, State> {
    source.state.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a state change asks to be done once the lock is dropped: arming,
/// queueing and releasing all take other locks, or run guest code.
#[derive(Default)]
struct Actions {
    arm: Option<(u64, u64)>,
    deliver: bool,
    cancel: Option<Option<Handler>>,
    release: Option<Handler>,
}

/// Decide what the source owes now, and mark it as owed.
fn settle(state: &mut State) -> Actions {
    let mut actions = Actions::default();
    if !state.running() {
        return actions;
    }
    if state.cancelled {
        if !state.cancel_submitted && !state.delivering {
            state.cancel_submitted = true;
            actions.release = state.event.take();
            actions.cancel = Some(state.cancel.take());
        }
        return actions;
    }
    if let Some(timer) = &state.timer
        && !state.armed
        && timer.target != time::FOREVER
    {
        state.armed = true;
        actions.arm = Some((timer.target, state.generation));
    }
    if state.pending > 0 && !state.delivering && state.event.is_some() {
        state.delivering = true;
        actions.deliver = true;
    }
    actions
}

fn perform(handle: u64, source: &Source, actions: Actions) {
    if let Some((deadline, generation)) = actions.arm {
        // The deadline holds a reference, so a source is not freed under it.
        object::retain(handle);
        let context = Box::into_raw(Box::new((handle, generation))) as u64;
        queue::arm_callback(deadline, tick, context);
    }
    if actions.deliver {
        object::retain(handle);
        queue::enqueue_function(source.target, deliver, handle);
    }
    if let Some(handler) = actions.release {
        handler.release();
    }
    match actions.cancel {
        Some(Some(Handler::Block(b))) => queue::async_owned(source.target, b),
        Some(Some(Handler::Function(f))) => {
            // SAFETY: a `dispatch_function_t` the guest installed.
            let f = unsafe { std::mem::transmute::<u64, Function>(f) };
            let context = unsafe { object::get_context(handle) } as u64;
            queue::enqueue_function(source.target, f, context);
        }
        _ => {}
    }
}

/// The timer thread's callback when an armed deadline comes due.
unsafe extern "C" fn tick(context: *mut c_void) {
    // SAFETY: the box `perform` leaked, handed back exactly once.
    let (handle, generation) = unsafe { *Box::from_raw(context as *mut (u64, u64)) };
    // SAFETY: the reference the deadline held.
    if let Some(source) = unsafe { source(handle) } {
        let actions = {
            let mut state = lock(source);
            if state.generation != generation || state.cancelled {
                Actions::default()
            } else {
                state.armed = false;
                let fired = state
                    .timer
                    .as_mut()
                    .map_or(0, |timer| fire(timer, time::now()));
                state.pending = state.pending.saturating_add(fired);
                settle(&mut state)
            }
        };
        perform(handle, source, actions);
    }
    // SAFETY: the reference the deadline held.
    unsafe { object::release(handle) };
}

/// How many times `timer` has fired by `now`, advancing it to its next deadline.
fn fire(timer: &mut Timer, now: u64) -> u64 {
    match timer.interval {
        None => {
            timer.target = time::FOREVER;
            1
        }
        Some(interval) => {
            let fired = 1 + now.saturating_sub(timer.target) / interval;
            timer.target = timer
                .target
                .saturating_add(fired.saturating_mul(interval))
                .min(time::FOREVER - 1);
            fired
        }
    }
}

/// Run the event handler on the target queue, once, for everything pending.
unsafe extern "C" fn deliver(context: *mut c_void) {
    let handle = context as u64;
    // SAFETY: the reference `perform` took for this delivery.
    let Some(source) = (unsafe { source(handle) }) else {
        return;
    };
    let run = {
        let mut state = lock(source);
        match state.event {
            Some(handler) if !state.cancelled && state.running() && state.pending > 0 => {
                state.data = state.pending;
                state.pending = 0;
                // A reference of its own, so a handler replaced while it runs
                // is not freed under it.
                match handler {
                    Handler::Block(b) => block::copy(b).map(Handler::Block),
                    function => Some(function),
                }
            }
            _ => None,
        }
    };
    match run {
        Some(Handler::Block(b)) => {
            // SAFETY: a heap block this module copied from the guest's.
            unsafe { block::invoke(b) };
            block::release(b);
        }
        Some(Handler::Function(f)) => {
            // SAFETY: a `dispatch_function_t`, called with the source's context.
            let f = unsafe { std::mem::transmute::<u64, Function>(f) };
            unsafe { f(object::get_context(handle)) };
        }
        None => {}
    }
    let actions = {
        let mut state = lock(source);
        state.delivering = false;
        settle(&mut state)
    };
    perform(handle, source, actions);
    // SAFETY: the reference `perform` took for this delivery.
    unsafe { object::release(handle) };
}

/// Apply `change` under the lock, then do what it left owed.
fn update(handle: u64, change: impl FnOnce(&mut State)) {
    // SAFETY: the guest holds a reference for the duration of the call.
    let Some(source) = (unsafe { source(handle) }) else {
        return;
    };
    let actions = {
        let mut state = lock(source);
        change(&mut state);
        settle(&mut state)
    };
    perform(handle, source, actions);
}

/// The queue a source's handlers run on, or zero for a handle that is not one.
pub(crate) fn target(handle: u64) -> u64 {
    // SAFETY: called with a live handle.
    unsafe { source(handle) }.map_or(0, |s| s.target)
}

pub(crate) fn suspend(handle: u64) {
    update(handle, |state| {
        state.suspended = state.suspended.saturating_add(1)
    });
}

/// Count the deadlines a source passed while it could not take them, so the
/// first delivery after a resume carries all of them rather than one.
fn catch_up(state: &mut State) {
    if !state.running() || state.cancelled || state.armed {
        return;
    }
    let now = time::now();
    if let Some(timer) = state.timer.as_mut()
        && timer.target != time::FOREVER
        && timer.target <= now
    {
        let fired = fire(timer, now);
        state.pending = state.pending.saturating_add(fired);
    }
}

/// The first resume of a new source activates it rather than undoing a suspend.
pub(crate) fn resume(handle: u64) {
    update(handle, |state| {
        if state.inactive {
            state.inactive = false;
        } else {
            state.suspended = state.suspended.saturating_sub(1);
        }
        catch_up(state);
    });
}

pub(crate) fn activate(handle: u64) {
    update(handle, |state| {
        state.inactive = false;
        catch_up(state);
    });
}

/// `dispatch_source_t dispatch_source_create(dispatch_source_type_t, uintptr_t handle,
/// uintptr_t mask, dispatch_queue_t)`.
///
/// Null for a type this module does not implement, which is the documented
/// answer for a source that could not be created. A null queue is the
/// default-priority global queue, as it is in libdispatch.
pub unsafe extern "C" fn create(kind: u64, handle: u64, mask: u64, target: u64) -> u64 {
    if kind != &raw const TYPE_TIMER as u64 {
        return 0;
    }
    let target = match target {
        // SAFETY: no memory is touched.
        0 => unsafe { queue::get_global_queue(0, 0) },
        q => q,
    };
    object::retain(target);
    let source = Box::new(Source {
        header: Header::new(Kind::Source),
        handle,
        mask,
        target,
        state: Mutex::new(State {
            inactive: true,
            suspended: 0,
            cancelled: false,
            cancel_submitted: false,
            event: None,
            cancel: None,
            timer: None,
            armed: false,
            generation: 0,
            pending: 0,
            data: 0,
            delivering: false,
        }),
    });
    Box::into_raw(source) as u64
}

/// `void dispatch_source_set_timer(dispatch_source_t, dispatch_time_t start,
/// uint64_t interval, uint64_t leeway)`.
///
/// libdispatch's normalisation: a zero interval is one nanosecond, and one past
/// `INT64_MAX` - `DISPATCH_TIME_FOREVER` among them - never repeats. A reset
/// discards what was pending. The leeway is a permission to fire late, which
/// firing on time already satisfies.
pub unsafe extern "C" fn set_timer(handle: u64, start: u64, interval: u64, _leeway: u64) {
    update(handle, |state| {
        if state.cancelled {
            return;
        }
        let target = match start {
            time::NOW => time::now(),
            t => t,
        };
        let interval = match interval {
            0 => Some(1),
            i if i >= i64::MAX as u64 => None,
            i => Some(i),
        };
        state.generation += 1;
        state.armed = false;
        state.pending = 0;
        state.timer = Some(Timer { target, interval });
    });
}

/// `void dispatch_source_cancel(dispatch_source_t)`.
pub unsafe extern "C" fn cancel(handle: u64) {
    update(handle, |state| {
        if !state.cancelled {
            state.cancelled = true;
            state.generation += 1;
            state.armed = false;
        }
    });
}

/// `intptr_t dispatch_source_testcancel(dispatch_source_t)`.
pub unsafe extern "C" fn testcancel(handle: u64) -> i64 {
    // SAFETY: the guest holds a reference.
    unsafe { source(handle) }.map_or(0, |s| i64::from(lock(s).cancelled))
}

/// `uintptr_t dispatch_source_get_data(dispatch_source_t)`.
pub unsafe extern "C" fn get_data(handle: u64) -> u64 {
    // SAFETY: the guest holds a reference.
    unsafe { source(handle) }.map_or(0, |s| lock(s).data)
}

/// `uintptr_t dispatch_source_get_handle(dispatch_source_t)`.
pub unsafe extern "C" fn get_handle(handle: u64) -> u64 {
    // SAFETY: the guest holds a reference.
    unsafe { source(handle) }.map_or(0, |s| s.handle)
}

/// `uintptr_t dispatch_source_get_mask(dispatch_source_t)`.
pub unsafe extern "C" fn get_mask(handle: u64) -> u64 {
    // SAFETY: the guest holds a reference.
    unsafe { source(handle) }.map_or(0, |s| s.mask)
}

/// Install `handler` in the slot `pick` names, releasing what was there.
fn install(handle: u64, handler: Option<Handler>, pick: fn(&mut State) -> &mut Option<Handler>) {
    let mut old = None;
    update(handle, |state| {
        old = std::mem::replace(pick(state), handler)
    });
    if let Some(old) = old {
        old.release();
    }
}

fn copied(block: u64) -> Option<Handler> {
    block::copy(block).map(Handler::Block)
}

fn function(f: u64) -> Option<Handler> {
    (f != 0).then_some(Handler::Function(f))
}

/// `void dispatch_source_set_event_handler(dispatch_source_t, dispatch_block_t)`.
/// The block is copied, and the one it replaces released.
pub unsafe extern "C" fn set_event_handler(handle: u64, handler: u64) {
    install(handle, copied(handler), |s| &mut s.event);
}

/// `void dispatch_source_set_event_handler_f(dispatch_source_t, dispatch_function_t)`.
pub unsafe extern "C" fn set_event_handler_f(handle: u64, handler: u64) {
    install(handle, function(handler), |s| &mut s.event);
}

/// `void dispatch_source_set_cancel_handler(dispatch_source_t, dispatch_block_t)`.
pub unsafe extern "C" fn set_cancel_handler(handle: u64, handler: u64) {
    install(handle, copied(handler), |s| &mut s.cancel);
}

/// `void dispatch_source_set_cancel_handler_f(dispatch_source_t, dispatch_function_t)`.
pub unsafe extern "C" fn set_cancel_handler_f(handle: u64, handler: u64) {
    install(handle, function(handler), |s| &mut s.cancel);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    unsafe extern "C" fn bump(context: *mut c_void) {
        // SAFETY: every test passes the address of a live `AtomicU32`.
        unsafe { &*(context as *const AtomicU32) }.fetch_add(1, Ordering::AcqRel);
    }

    fn wait_for(counter: &AtomicU32, target: u32) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while counter.load(Ordering::Acquire) < target {
            assert!(Instant::now() < deadline, "the source never fired");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A timer source on a private serial queue, counting into `counter`.
    fn timer(counter: &AtomicU32, start_ms: i64, interval: u64) -> u64 {
        let q = queue::create_serial("source test");
        // SAFETY: a live queue and a counter that outlives the source.
        unsafe {
            let s = create(&raw const TYPE_TIMER as u64, 0, 0, q);
            object::release(q);
            object::set_context(s, (counter as *const AtomicU32).cast_mut().cast());
            set_event_handler_f(s, bump as *const () as usize as u64);
            set_timer(s, time::time(time::NOW, start_ms * 1_000_000), interval, 0);
            s
        }
    }

    #[test]
    fn a_new_source_does_nothing_until_resumed() {
        let counter = AtomicU32::new(0);
        let s = timer(&counter, 0, 5_000_000);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(counter.load(Ordering::Acquire), 0, "fired while inactive");
        // SAFETY: a live source.
        unsafe { object::resume_entry(s) };
        wait_for(&counter, 2);
        unsafe {
            cancel(s);
            object::release(s);
        }
    }

    #[test]
    fn a_one_shot_timer_fires_once_after_its_start() {
        let counter = AtomicU32::new(0);
        let s = timer(&counter, 30, time::FOREVER);
        // SAFETY: a live source.
        unsafe { object::activate_entry(s) };
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(counter.load(Ordering::Acquire), 0, "fired early");
        wait_for(&counter, 1);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(counter.load(Ordering::Acquire), 1, "a one-shot repeated");
        unsafe { object::release(s) };
    }

    static CANCELLED: AtomicU32 = AtomicU32::new(0);

    unsafe extern "C" fn note_cancel(_context: *mut c_void) {
        CANCELLED.fetch_add(1, Ordering::AcqRel);
    }

    #[test]
    fn cancel_stops_events_and_runs_the_cancel_handler_once() {
        let counter = AtomicU32::new(0);
        let s = timer(&counter, 0, 2_000_000);
        // SAFETY: a live source.
        unsafe {
            set_cancel_handler_f(s, note_cancel as *const () as usize as u64);
            object::resume_entry(s);
        }
        wait_for(&counter, 3);
        unsafe { cancel(s) };
        assert_ne!(unsafe { testcancel(s) }, 0);
        wait_for(&CANCELLED, 1);
        let seen = counter.load(Ordering::Acquire);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(counter.load(Ordering::Acquire), seen, "fired after cancel");
        // A second cancel is not a second cancel handler, nor is a resume.
        unsafe {
            cancel(s);
            object::resume_entry(s);
        }
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(CANCELLED.load(Ordering::Acquire), 1);
        unsafe { object::release(s) };
    }

    static MOST_DATA: AtomicU64 = AtomicU64::new(0);
    static SOURCE: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C" fn record(context: *mut c_void) {
        let data = unsafe { get_data(SOURCE.load(Ordering::Acquire)) };
        MOST_DATA.fetch_max(data, Ordering::AcqRel);
        unsafe { bump(context) };
    }

    #[test]
    fn a_reset_discards_events_still_pending() {
        let counter = AtomicU32::new(0);
        let s = timer(&counter, 0, 1_000_000);
        // SAFETY: a live source.
        unsafe { object::resume_entry(s) };
        wait_for(&counter, 1);
        unsafe { object::suspend_entry(s) };
        std::thread::sleep(Duration::from_millis(10));
        let before = counter.load(Ordering::Acquire);
        // Deadlines have passed while suspended; the reset forgets them.
        std::thread::sleep(Duration::from_millis(10));
        unsafe {
            set_timer(s, time::FOREVER, time::FOREVER, 0);
            object::resume_entry(s);
        }
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(counter.load(Ordering::Acquire), before);
        unsafe { object::release(s) };
    }

    #[test]
    fn events_while_suspended_arrive_as_one_delivery_with_their_count() {
        let counter = AtomicU32::new(0);
        let s = timer(&counter, 0, 2_000_000);
        SOURCE.store(s, Ordering::Release);
        // SAFETY: a live source.
        unsafe {
            set_event_handler_f(s, record as *const () as usize as u64);
            object::resume_entry(s);
        }
        wait_for(&counter, 1);
        unsafe { object::suspend_entry(s) };
        // Let any delivery already queued land before counting.
        std::thread::sleep(Duration::from_millis(10));
        let before = counter.load(Ordering::Acquire);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(
            counter.load(Ordering::Acquire),
            before,
            "fired while suspended"
        );
        unsafe { object::resume_entry(s) };
        wait_for(&counter, before + 1);
        assert!(
            MOST_DATA.load(Ordering::Acquire) > 1,
            "the missed intervals were not coalesced into the data"
        );
        unsafe {
            cancel(s);
            object::release(s);
        }
    }

    #[test]
    fn only_the_timer_type_can_be_created() {
        // SAFETY: no handle is made, so nothing is touched.
        assert_eq!(unsafe { create(0x1000, 0, 0, 0) }, 0);
        let s = unsafe { create(&raw const TYPE_TIMER as u64, 7, 1, 0) };
        assert_ne!(s, 0);
        unsafe {
            assert_eq!(get_handle(s), 7);
            assert_eq!(get_mask(s), 1);
            assert_eq!(testcancel(s), 0);
            object::release(s);
        }
    }

    #[test]
    fn a_reset_timer_forgets_its_old_deadline() {
        let counter = AtomicU32::new(0);
        let s = timer(&counter, 20, time::FOREVER);
        // SAFETY: a live source.
        unsafe {
            object::resume_entry(s);
            set_timer(s, time::FOREVER, time::FOREVER, 0);
        }
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(counter.load(Ordering::Acquire), 0, "the old deadline fired");
        unsafe { object::release(s) };
    }

    #[test]
    fn a_missed_deadline_counts_every_interval_it_skipped() {
        let mut timer = Timer {
            target: 1_000,
            interval: Some(100),
        };
        assert_eq!(fire(&mut timer, 1_350), 4);
        assert_eq!(timer.target, 1_400);
    }
}
