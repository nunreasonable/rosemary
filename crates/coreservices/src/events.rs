use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

type OSStatus = i32;

const NO_ERR: OSStatus = 0;
/// `eventLoopTimedOutErr`: nothing arrived before the timeout.
const EVENT_LOOP_TIMED_OUT_ERR: OSStatus = -9875;
/// `eventNotHandledErr`: no handler claimed the event.
const EVENT_NOT_HANDLED_ERR: OSStatus = -9874;
/// `eventParameterNotFoundErr`, which is also what Carbon answers for a
/// reference that names nothing.
const EVENT_INVALID_PARAMETER: OSStatus = -50;

/// The singletons a program asks for by name. Distinct non-null addresses, so
/// that a caller comparing two targets gets the right answer; the bytes behind
/// them are never read by anything here.
static APPLICATION_TARGET: u8 = 0;
static DISPATCHER_TARGET: u8 = 0;
static MAIN_EVENT_LOOP: u8 = 0;

/// Handler and timer references handed out and not yet removed.
///
/// Kept so that removing one can say whether it was ever installed, which is
/// the only thing a caller can observe about a handler that is never called.
static INSTALLED: Mutex<Option<HashSet<u64>>> = Mutex::new(None);

/// References are odd-numbered and start high, because a program that stores
/// one in a field it also uses for a Cocoa pointer is easier to spot that way
/// than if they were 1, 2, 3.
static NEXT_REFERENCE: AtomicU64 = AtomicU64::new(0x4361_7262_0001);

fn install() -> u64 {
    let reference = NEXT_REFERENCE.fetch_add(2, Ordering::Relaxed);
    let mut held = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
    held.get_or_insert_with(HashSet::new).insert(reference);
    reference
}

fn uninstall(reference: u64) -> bool {
    let mut held = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
    held.as_mut().is_some_and(|table| table.remove(&reference))
}

/// `EventTargetRef GetApplicationEventTarget(void)`
pub unsafe extern "C" fn GetApplicationEventTarget() -> *mut c_void {
    (&raw const APPLICATION_TARGET).cast_mut().cast()
}

/// `EventTargetRef GetEventDispatcherTarget(void)`
pub unsafe extern "C" fn GetEventDispatcherTarget() -> *mut c_void {
    (&raw const DISPATCHER_TARGET).cast_mut().cast()
}

/// `EventLoopRef GetMainEventLoop(void)`
pub unsafe extern "C" fn GetMainEventLoop() -> *mut c_void {
    (&raw const MAIN_EVENT_LOOP).cast_mut().cast()
}

/// `EventTime GetCurrentEventTime(void)`
///
/// Seconds since boot as a `double`, which is the same clock
/// `mach_absolute_time` reports and the one a program subtracts two readings
/// of to get a frame time.
pub unsafe extern "C" fn GetCurrentEventTime() -> f64 {
    darwin::mach::media_time()
}

/// `OSStatus InstallEventHandler(EventTargetRef inTarget, EventHandlerUPP
/// inHandler, ItemCount inNumTypes, const EventTypeSpec *inList, void
/// *inUserData, EventHandlerRef *outRef)`
///
/// # Safety
///
/// `out` is null or a writable `EventHandlerRef`.
pub unsafe extern "C" fn InstallEventHandler(
    _target: *mut c_void,
    _handler: *mut c_void,
    _types: u64,
    _list: *const c_void,
    _context: *mut c_void,
    out: *mut u64,
) -> OSStatus {
    let reference = install();
    if !out.is_null() {
        // SAFETY: the caller states it is writable.
        unsafe { *out = reference };
    }
    NO_ERR
}

/// `OSStatus RemoveEventHandler(EventHandlerRef inHandlerRef)`
pub unsafe extern "C" fn RemoveEventHandler(reference: u64) -> OSStatus {
    match uninstall(reference) {
        true => NO_ERR,
        false => EVENT_INVALID_PARAMETER,
    }
}

/// `OSStatus InstallEventLoopTimer(EventLoopRef inEventLoop, EventTimerInterval
/// inFireDelay, EventTimerInterval inInterval, EventLoopTimerUPP inTimerProc,
/// void *inTimerData, EventLoopTimerRef *outTimer)`
///
/// The timer is recorded and never fires: nothing here drives a Carbon event
/// loop, and a program that depends on one would be waiting on this tree
/// rather than on its own clock.
///
/// # Safety
///
/// `out` is null or a writable `EventLoopTimerRef`.
pub unsafe extern "C" fn InstallEventLoopTimer(
    _loop_ref: *mut c_void,
    _delay: f64,
    _interval: f64,
    _proc: *mut c_void,
    _context: *mut c_void,
    out: *mut u64,
) -> OSStatus {
    let reference = install();
    if !out.is_null() {
        // SAFETY: the caller states it is writable.
        unsafe { *out = reference };
    }
    NO_ERR
}

/// `OSStatus RemoveEventLoopTimer(EventLoopTimerRef inTimer)`
pub unsafe extern "C" fn RemoveEventLoopTimer(reference: u64) -> OSStatus {
    match uninstall(reference) {
        true => NO_ERR,
        false => EVENT_INVALID_PARAMETER,
    }
}

/// `OSStatus ReceiveNextEvent(ItemCount inNumTypes, const EventTypeSpec
/// *inList, EventTimeout inTimeout, Boolean inPullEvent, EventRef *outEvent)`
///
/// The queue is empty, so this is the timeout answer. The timeout itself is
/// *not* waited out: a caller passing `kEventDurationForever` means "until
/// something happens", and nothing ever will, so sleeping on it would hang the
/// program rather than idle it.
///
/// # Safety
///
/// `out` is null or a writable `EventRef`.
pub unsafe extern "C" fn ReceiveNextEvent(
    _types: u64,
    _list: *const c_void,
    _timeout: f64,
    _pull: u8,
    out: *mut *mut c_void,
) -> OSStatus {
    if !out.is_null() {
        // SAFETY: the caller states it is writable.
        unsafe { *out = std::ptr::null_mut() };
    }
    EVENT_LOOP_TIMED_OUT_ERR
}

/// `void ReleaseEvent(EventRef inEvent)`
///
/// Nothing here makes an event, so nothing here owns one.
pub unsafe extern "C" fn ReleaseEvent(_event: *mut c_void) {}

/// `OSStatus SendEventToEventTarget(EventRef inEvent, EventTargetRef inTarget)`
pub unsafe extern "C" fn SendEventToEventTarget(
    _event: *mut c_void,
    _target: *mut c_void,
) -> OSStatus {
    EVENT_NOT_HANDLED_ERR
}

/// `OSStatus SetSystemUIMode(SystemUIMode inMode, SystemUIOptions inOptions)`
///
/// Hiding the menu bar and the Dock for a full-screen game. There is neither
/// here, and the window's own full-screen state is AppKit's - so this is
/// accepted and has nothing to do.
pub unsafe extern "C" fn SetSystemUIMode(_mode: u32, _options: u32) -> OSStatus {
    NO_ERR
}

/// `UInt32 GetEventKind(EventRef)`
///
/// Zero: nothing here makes an `EventRef` (see [`ReceiveNextEvent`]), so there
/// is no event whose kind could be asked for.
pub unsafe extern "C" fn GetEventKind(_event: *const c_void) -> u32 {
    0
}

/// `OSStatus EnableSecureEventInput(void)` and its pair.
///
/// Secure input keeps other processes from reading keystrokes; no process
/// here can, so there is nothing to turn on.
pub unsafe extern "C" fn EnableSecureEventInput() -> OSStatus {
    NO_ERR
}

pub unsafe extern "C" fn DisableSecureEventInput() -> OSStatus {
    NO_ERR
}

/// `TSMDocumentID TSMGetActiveDocument(void)`
///
/// NULL: Cocoa text input goes through `NSTextInputContext`, and no Carbon
/// text services document is ever activated.
pub unsafe extern "C" fn TSMGetActiveDocument() -> *mut c_void {
    std::ptr::null_mut()
}

/// `OSStatus TSMSetDocumentProperty(TSMDocumentID, OSType, UInt32, void *)`
/// and `TSMRemoveDocumentProperty(TSMDocumentID, OSType)`.
///
/// `tsmInvalidDocIDErr` for the NULL document, which is the only one
/// [`TSMGetActiveDocument`] answers.
pub unsafe extern "C" fn TSMSetDocumentProperty(
    _document: *mut c_void,
    _tag: u32,
    _size: u32,
    _data: *const c_void,
) -> OSStatus {
    TSM_INVALID_DOC_ID_ERR
}

pub unsafe extern "C" fn TSMRemoveDocumentProperty(_document: *mut c_void, _tag: u32) -> OSStatus {
    TSM_INVALID_DOC_ID_ERR
}

/// `tsmInvalidDocIDErr`.
const TSM_INVALID_DOC_ID_ERR: OSStatus = -2507;

exports::exports! {
    functions {
        "GetEventKind" => GetEventKind,
        "EnableSecureEventInput" => EnableSecureEventInput,
        "DisableSecureEventInput" => DisableSecureEventInput,
        "TSMGetActiveDocument" => TSMGetActiveDocument,
        "TSMSetDocumentProperty" => TSMSetDocumentProperty,
        "TSMRemoveDocumentProperty" => TSMRemoveDocumentProperty,
        "GetApplicationEventTarget" => GetApplicationEventTarget,
        "GetEventDispatcherTarget" => GetEventDispatcherTarget,
        "GetMainEventLoop" => GetMainEventLoop,
        "GetCurrentEventTime" => GetCurrentEventTime,
        "InstallEventHandler" => InstallEventHandler,
        "RemoveEventHandler" => RemoveEventHandler,
        "InstallEventLoopTimer" => InstallEventLoopTimer,
        "RemoveEventLoopTimer" => RemoveEventLoopTimer,
        "ReceiveNextEvent" => ReceiveNextEvent,
        "ReleaseEvent" => ReleaseEvent,
        "SendEventToEventTarget" => SendEventToEventTarget,
        "SetSystemUIMode" => SetSystemUIMode,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three singletons are non-null and are three different things, which
    /// is what a program comparing two targets depends on.
    #[test]
    fn the_targets_are_distinct_and_not_null() {
        // SAFETY: none of the three reads anything.
        let (application, dispatcher, main) = unsafe {
            (
                GetApplicationEventTarget(),
                GetEventDispatcherTarget(),
                GetMainEventLoop(),
            )
        };
        assert!(!application.is_null() && !dispatcher.is_null() && !main.is_null());
        assert_ne!(application, dispatcher);
        assert_ne!(application, main);
    }

    /// A receive on an empty queue times out and leaves no event behind - the
    /// caller would release whatever was written there.
    #[test]
    fn a_receive_times_out_and_writes_no_event() {
        // Not null to begin with, so that the call having written null is
        // something this can tell from it having written nothing.
        let mut event = std::ptr::without_provenance_mut::<c_void>(1);
        // SAFETY: a writable out-parameter.
        let status = unsafe { ReceiveNextEvent(0, std::ptr::null(), f64::MAX, 1, &mut event) };
        assert_eq!(status, EVENT_LOOP_TIMED_OUT_ERR);
        assert!(event.is_null());
    }

    /// A handler can be removed once, and a reference that names nothing is
    /// refused rather than reported as removed.
    #[test]
    fn a_handler_is_removed_once() {
        let mut reference = 0_u64;
        // SAFETY: a writable out-parameter.
        let status = unsafe {
            InstallEventHandler(
                GetApplicationEventTarget(),
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
                std::ptr::null_mut(),
                &mut reference,
            )
        };
        assert_eq!(status, NO_ERR);
        assert_ne!(reference, 0);
        unsafe {
            assert_eq!(RemoveEventHandler(reference), NO_ERR);
            assert_eq!(RemoveEventHandler(reference), EVENT_INVALID_PARAMETER);
        }
    }

    /// The event time moves forward, because a program divides by the
    /// difference between two readings.
    #[test]
    fn the_event_time_advances() {
        let first = unsafe { GetCurrentEventTime() };
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = unsafe { GetCurrentEventTime() };
        assert!(second > first, "{second} is not after {first}");
    }
}
