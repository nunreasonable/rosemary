#![allow(clippy::manual_dangling_ptr, clippy::missing_safety_doc)]
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// `CVReturn`. Zero is success, and the one failure this reports is
/// `kCVReturnInvalidArgument`.
const SUCCESS: i32 = 0;
const INVALID_ARGUMENT: i32 = -6661;

/// `CVTimeStamp`, which the callback receives twice - "now" and "when this frame
/// will be shown".
///
/// Eighty bytes, and the layout is ABI: a guest reads `videoTime` and
/// `videoTimeScale` out of it to work out how much time has passed. Checked
/// against the header rather than counted.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct TimeStamp {
    pub version: u32,
    pub video_time_scale: i32,
    pub video_time: i64,
    pub host_time: u64,
    pub rate_scalar: f64,
    pub video_refresh_period: i64,
    /// `CVSMPTETime`, which nothing here fills in.
    smpte_time: [u8; 24],
    pub flags: u64,
    pub reserved: u64,
}

const _: () = {
    assert!(size_of::<TimeStamp>() == 80);
    assert!(std::mem::offset_of!(TimeStamp, video_time) == 8);
    assert!(std::mem::offset_of!(TimeStamp, host_time) == 16);
    assert!(std::mem::offset_of!(TimeStamp, rate_scalar) == 24);
    assert!(std::mem::offset_of!(TimeStamp, video_refresh_period) == 32);
    assert!(std::mem::offset_of!(TimeStamp, flags) == 64);
};

/// `CVDisplayLinkOutputCallback`.
type OutputCallback = unsafe extern "C" fn(
    *mut c_void,
    *const TimeStamp,
    *const TimeStamp,
    u64,
    *mut u64,
    *mut c_void,
) -> i32;

/// One display link.
struct Link {
    callback: Option<OutputCallback>,
    context: *mut c_void,
    /// Set while the link's thread should keep calling back. Cleared by
    /// [`CVDisplayLinkStop`], which is the thread's only stop signal.
    running: Option<Arc<AtomicBool>>,
    /// Seconds between frames, from the display SDL reports.
    period: f64,
    /// How many times the callback has run, which is the `videoTime` a guest
    /// integrates to get elapsed time.
    frame: i64,
    started: std::time::Instant,
}

// SAFETY: the context is the guest's own pointer and is only ever handed back to
// the guest's callback; the callback is a thunk this runtime made.
unsafe impl Send for Link {}

/// Every link this process has made, by the handle the guest holds.
///
/// The handle is the address of the `Arc`'s contents, which is stable and unique
/// for as long as the link is alive - so it is the `CVDisplayLinkRef` itself
/// rather than an index a guest could fabricate.
static LINKS: Mutex<Vec<(usize, Arc<Mutex<Link>>)>> = Mutex::new(Vec::new());

fn link_for(handle: *mut c_void) -> Option<Arc<Mutex<Link>>> {
    LINKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(h, _)| *h == handle as usize)
        .map(|(_, link)| Arc::clone(link))
}

/// The refresh period of a display, in seconds.
///
/// A zero display names the primary display for the active-displays spelling.
/// Sixty hertz is only a fallback for a host with no SDL video subsystem or no
/// desktop mode; a real display link follows the mode SDL reports.
fn display_period(display: u32) -> f64 {
    let display = match display {
        0 => sdl3::video::primary_display(),
        display => display,
    };
    let refresh_rate = sdl3::video::display_refresh_rate(display).unwrap_or(60.0);
    1.0 / f64::from(refresh_rate)
}

/// `CVReturn CVDisplayLinkCreateWithCGDisplay(CGDirectDisplayID, CVDisplayLinkRef *)`.
///
/// # Safety
///
/// `out` must be null or a writable pointer-sized slot.
pub unsafe extern "C" fn CVDisplayLinkCreateWithCGDisplay(
    display: u32,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() {
        return INVALID_ARGUMENT;
    }
    let link = Arc::new(Mutex::new(Link {
        callback: None,
        context: std::ptr::null_mut(),
        running: None,
        period: display_period(display),
        frame: 0,
        started: std::time::Instant::now(),
    }));
    // The handle is the `Mutex`'s address, which the `Arc` in the table keeps
    // alive for as long as the link exists.
    let handle = Arc::as_ptr(&link) as *mut c_void;
    LINKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((handle as usize, link));
    unsafe { *out = handle };
    SUCCESS
}

/// As above, for the "all active displays" spelling. One display link either
/// way: this runtime draws into one window.
///
/// # Safety
///
/// As [`CVDisplayLinkCreateWithCGDisplay`].
pub unsafe extern "C" fn CVDisplayLinkCreateWithActiveCGDisplays(out: *mut *mut c_void) -> i32 {
    unsafe { CVDisplayLinkCreateWithCGDisplay(0, out) }
}

/// `CVReturn CVDisplayLinkCreateWithCGDisplays(const CGDirectDisplayID *displayArray,
/// CFIndex count, CVDisplayLinkRef *displayLinkOut)`.
///
/// Also one display link, on this runtime's one window: the period comes from
/// the first display of the set, and the rest of the work is
/// [`CVDisplayLinkCreateWithCGDisplay`]'s.
///
/// # Safety
///
/// `display_array` must be readable for at least `count` `u32`s when `count`
/// is positive; `out` as [`CVDisplayLinkCreateWithCGDisplay`].
pub unsafe extern "C" fn CVDisplayLinkCreateWithCGDisplays(
    display_array: *const u32,
    count: i64,
    out: *mut *mut c_void,
) -> i32 {
    if display_array.is_null() || count <= 0 {
        return INVALID_ARGUMENT;
    }
    // SAFETY: the caller's array, at least `count` long by the function's own
    // contract, so its first element is there to read.
    let display = unsafe { *display_array };
    unsafe { CVDisplayLinkCreateWithCGDisplay(display, out) }
}

/// # Safety
///
/// `callback` must be a host-callable function of the display link's prototype -
/// which for a guest's own function is the thunk `rosemary`'s ABI layer made,
/// because the table entry names the parameter a callback.
pub unsafe extern "C" fn CVDisplayLinkSetOutputCallback(
    handle: *mut c_void,
    callback: *mut c_void,
    context: *mut c_void,
) -> i32 {
    let Some(link) = link_for(handle) else {
        return INVALID_ARGUMENT;
    };
    let mut link = link.lock().unwrap_or_else(|e| e.into_inner());
    link.callback = (!callback.is_null())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, OutputCallback>(callback) });
    link.context = context;
    SUCCESS
}

pub unsafe extern "C" fn CVDisplayLinkSetCurrentCGDisplay(
    handle: *mut c_void,
    display: u32,
) -> i32 {
    let Some(link) = link_for(handle) else {
        return INVALID_ARGUMENT;
    };
    link.lock().unwrap_or_else(|e| e.into_inner()).period = display_period(display);
    SUCCESS
}

pub unsafe extern "C" fn CVDisplayLinkSetCurrentCGDisplayFromOpenGLContext(
    handle: *mut c_void,
    _context: *mut c_void,
    _format: *mut c_void,
) -> i32 {
    match link_for(handle).is_some() {
        true => SUCCESS,
        false => INVALID_ARGUMENT,
    }
}

pub unsafe extern "C" fn CVDisplayLinkGetActualOutputVideoRefreshPeriod(
    handle: *mut c_void,
) -> f64 {
    link_for(handle).map_or(0.0, |link| {
        link.lock().unwrap_or_else(|e| e.into_inner()).period
    })
}

/// `CVTime`: sixteen bytes, so both ABIs return it in two integer registers.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Time {
    pub time_value: i64,
    pub time_scale: i32,
    pub flags: i32,
}

/// `kCVTimeIsIndefinite`, for a handle that names no link.
const TIME_IS_INDEFINITE: i32 = 1;

/// The scale the nominal period is reported in, nanoseconds.
const NOMINAL_SCALE: i32 = 1_000_000_000;

/// `CVTime CVDisplayLinkGetNominalOutputVideoRefreshPeriod(CVDisplayLinkRef)`
///
/// Unity 5 derives its frame rate cap from this; zero reads as "invalid".
pub unsafe extern "C" fn CVDisplayLinkGetNominalOutputVideoRefreshPeriod(
    handle: *mut c_void,
) -> Time {
    match link_for(handle) {
        Some(link) => {
            let period = link.lock().unwrap_or_else(|e| e.into_inner()).period;
            Time {
                time_value: (period * f64::from(NOMINAL_SCALE)).round() as i64,
                time_scale: NOMINAL_SCALE,
                flags: 0,
            }
        }
        None => Time {
            time_value: 0,
            time_scale: 0,
            flags: TIME_IS_INDEFINITE,
        },
    }
}

/// `CVDisplayLinkStart`: start the thread that calls the guest back.
pub unsafe extern "C" fn CVDisplayLinkStart(handle: *mut c_void) -> i32 {
    let Some(link) = link_for(handle) else {
        return INVALID_ARGUMENT;
    };
    let (period, running) = {
        let mut guard = link.lock().unwrap_or_else(|e| e.into_inner());
        if guard.running.is_some() {
            return SUCCESS;
        }
        guard.started = std::time::Instant::now();
        let running = Arc::new(AtomicBool::new(true));
        guard.running = Some(Arc::clone(&running));
        (guard.period, running)
    };

    let period = std::time::Duration::from_secs_f64(period);
    let at = handle as usize;
    // Detached. A guest may stop its link from inside the callback, and a
    // `Stop` that joined would then be the thread waiting for itself.
    std::thread::Builder::new()
        .name("CVDisplayLink".to_string())
        .spawn(move || {
            // The callback is guest code, which reads its thread identity off the
            // thread pointer - its parent's on a new thread until it enters.
            let _darwin = darwin::tls::DarwinThread::enter();
            let mut next = std::time::Instant::now() + period;
            while running.load(Ordering::Acquire) {
                let now = std::time::Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else {
                    // Behind: give up the frames that were missed rather than
                    // calling back in a burst to catch up.
                    next = now;
                }
                next += period;
                if !running.load(Ordering::Acquire) {
                    break;
                }
                // SAFETY: `at` is a handle this module made, and the table keeps
                // it alive.
                unsafe { fire(std::ptr::null(), at as *mut c_void) };
            }
        })
        .map_or(INVALID_ARGUMENT, |_| SUCCESS)
}

pub unsafe extern "C" fn CVDisplayLinkStop(handle: *mut c_void) -> i32 {
    let Some(link) = link_for(handle) else {
        return INVALID_ARGUMENT;
    };
    let running = link
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .running
        .take();
    if let Some(running) = running {
        running.store(false, Ordering::Release);
    }
    SUCCESS
}

pub unsafe extern "C" fn CVDisplayLinkRetain(handle: *mut c_void) -> *mut c_void {
    handle
}

pub unsafe extern "C" fn CVDisplayLinkRelease(_handle: *mut c_void) {}

pub unsafe extern "C" fn CVDisplayLinkIsRunning(handle: *mut c_void) -> u8 {
    link_for(handle).is_some_and(|link| {
        link.lock()
            .unwrap_or_else(|e| e.into_inner())
            .running
            .is_some()
    }) as u8
}

/// `Float64 CVGetHostClockFrequency(void)`.
///
/// The frequency of the clock a `CVGetCurrentHostTime` here would read: the
/// same nanosecond `CLOCK_MONOTONIC` tick `mach_absolute_time` uses, where
/// `mach_timebase_info`'s `numer`/`denom` are both one because a tick already
/// *is* a nanosecond on this host (see `darwin::mach`). A guest dividing that
/// clock's ticks by this answer gets seconds, which is the only thing either
/// function promises.
pub unsafe extern "C" fn CVGetHostClockFrequency() -> f64 {
    1e9
}

/// `CVPixelBufferReleaseBytesCallback`.
type ReleaseBytesCallback = unsafe extern "C" fn(*mut c_void, *const c_void);

/// A `CVPixelBuffer` made by `CVPixelBufferCreateWithBytes`: the caller's own
/// memory, never copied, released back to the caller - via `releaseCallback` -
/// only once nothing holds a reference any more.
struct PixelBuffer {
    base_address: *mut c_void,
    release_callback: Option<ReleaseBytesCallback>,
    release_ref_con: *mut c_void,
}

// SAFETY: `base_address` and `release_ref_con` are the caller's own pointers,
// handed back only to the caller's own release callback and never read here.
unsafe impl Send for PixelBuffer {}

impl Drop for PixelBuffer {
    fn drop(&mut self) {
        if let Some(callback) = self.release_callback {
            // SAFETY: `callback` is the guest's own release callback, run
            // exactly once - when the last reference goes away - which is
            // what `CVPixelBufferCreateWithBytes` promises its caller.
            unsafe { callback(self.release_ref_con, self.base_address) };
        }
    }
}

/// Every pixel buffer this process has made, by the handle the guest holds -
/// the `CVBuffer` family's own table, parallel to [`LINKS`].
///
/// One entry per outstanding reference rather than one entry with a count
/// field: [`CVBufferRetain`] pushes another clone of the same `Arc` and
/// [`CVBufferRelease`] drops one, so the `Arc`'s own strong count - not
/// something this module tracks by hand - is the reference count, and
/// [`PixelBuffer::drop`] runs exactly when it reaches zero.
static PIXEL_BUFFERS: Mutex<Vec<(usize, Arc<Mutex<PixelBuffer>>)>> = Mutex::new(Vec::new());

fn pixel_buffer_for(handle: *mut c_void) -> Option<Arc<Mutex<PixelBuffer>>> {
    PIXEL_BUFFERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(h, _)| *h == handle as usize)
        .map(|(_, buffer)| Arc::clone(buffer))
}

/// `CVReturn CVPixelBufferCreateWithBytes(CFAllocatorRef allocator, size_t width,
/// size_t height, OSType pixelFormatType, void *baseAddress, size_t bytesPerRow,
/// CVPixelBufferReleaseBytesCallback releaseCallback, void *releaseRefCon,
/// CFDictionaryRef pixelBufferAttributes, CVPixelBufferRef *pixelBufferOut)`.
///
/// Wraps `baseAddress` rather than copying it, as the real function does: the
/// pixels stay the caller's own memory until `releaseCallback` runs, on the
/// last [`CVBufferRelease`]/[`CVPixelBufferRelease`].
///
/// # Safety
///
/// `pixel_buffer_out` must be a writable pointer-sized slot; `base_address`
/// must stay valid until `release_callback` runs (or forever, if
/// `release_callback` is null).
pub unsafe extern "C" fn CVPixelBufferCreateWithBytes(
    _allocator: *const c_void,
    _width: u64,
    _height: u64,
    _pixel_format_type: u32,
    base_address: *mut c_void,
    _bytes_per_row: u64,
    release_callback: Option<ReleaseBytesCallback>,
    release_ref_con: *mut c_void,
    _pixel_buffer_attributes: *const c_void,
    pixel_buffer_out: *mut *mut c_void,
) -> i32 {
    if pixel_buffer_out.is_null() || base_address.is_null() {
        return INVALID_ARGUMENT;
    }
    let buffer = Arc::new(Mutex::new(PixelBuffer {
        base_address,
        release_callback,
        release_ref_con,
    }));
    // The handle is the `Mutex`'s address, as `CVDisplayLinkCreateWithCGDisplay`'s is.
    let handle = Arc::as_ptr(&buffer) as *mut c_void;
    PIXEL_BUFFERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((handle as usize, buffer));
    unsafe { *pixel_buffer_out = handle };
    SUCCESS
}

/// `CVReturn CVPixelBufferLockBaseAddress(CVPixelBufferRef pixelBuffer,
/// CVPixelBufferLockFlags lockFlags)`.
///
/// Nothing here ever moves or recycles the backing bytes, so there is nothing
/// a lock need defend: every call naming a buffer this module made succeeds.
pub unsafe extern "C" fn CVPixelBufferLockBaseAddress(
    pixel_buffer: *mut c_void,
    _lock_flags: u64,
) -> i32 {
    match pixel_buffer_for(pixel_buffer) {
        Some(_) => SUCCESS,
        None => INVALID_ARGUMENT,
    }
}

/// `CVReturn CVPixelBufferUnlockBaseAddress(CVPixelBufferRef pixelBuffer,
/// CVPixelBufferLockFlags unlockFlags)`. See [`CVPixelBufferLockBaseAddress`].
pub unsafe extern "C" fn CVPixelBufferUnlockBaseAddress(
    pixel_buffer: *mut c_void,
    _unlock_flags: u64,
) -> i32 {
    match pixel_buffer_for(pixel_buffer) {
        Some(_) => SUCCESS,
        None => INVALID_ARGUMENT,
    }
}

/// `void CVPixelBufferRelease(CVPixelBufferRef texture)` - `CFRelease`,
/// specialised to a pixel buffer. Not thread-safe on real Core Video;
/// [`CVBufferRelease`], which this forwards to, always takes a lock anyway.
///
/// # Safety
///
/// `texture` must be null or a handle this module handed out.
pub unsafe extern "C" fn CVPixelBufferRelease(texture: *mut c_void) {
    unsafe { CVBufferRelease(texture) }
}

/// `CVBufferRef CVBufferRetain(CVBufferRef buffer)`.
///
/// Null-safe, unlike `CFRetain`. The only `CVBuffer` family this runtime
/// makes is [`PixelBuffer`], so that is the table this looks in.
pub unsafe extern "C" fn CVBufferRetain(buffer: *mut c_void) -> *mut c_void {
    if let Some(buffer_ref) = pixel_buffer_for(buffer) {
        PIXEL_BUFFERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((buffer as usize, buffer_ref));
    }
    buffer
}

/// `void CVBufferRelease(CVBufferRef buffer)`.
///
/// Null-safe, unlike `CFRelease`. Drops one reference; [`PixelBuffer::drop`]
/// runs the release callback when that was the last one.
pub unsafe extern "C" fn CVBufferRelease(buffer: *mut c_void) {
    if buffer.is_null() {
        return;
    }
    let mut table = PIXEL_BUFFERS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pos) = table.iter().position(|(h, _)| *h == buffer as usize) {
        table.remove(pos);
    }
}

/// One turn of the link: build the two timestamps and call the guest.
///
/// `info` is the link's handle, which is how one function serves them all.
unsafe extern "C" fn fire(_timer: *const c_void, info: *mut c_void) {
    let Some(link) = link_for(info) else {
        return;
    };
    let (callback, context, now, output) = {
        let mut guard = link.lock().unwrap_or_else(|e| e.into_inner());
        let Some(callback) = guard.callback else {
            return;
        };
        guard.frame += 1;
        let scale = 1_000_000_000i32;
        let elapsed = guard.started.elapsed().as_secs_f64();
        let now = TimeStamp {
            version: 0,
            video_time_scale: scale,
            video_time: (elapsed * scale as f64) as i64,
            host_time: elapsed.to_bits(),
            rate_scalar: 1.0,
            video_refresh_period: (guard.period * scale as f64) as i64,
            smpte_time: [0; 24],
            flags: 0,
            reserved: 0,
        };
        // The frame after this one, which is what "output time" means.
        let output = TimeStamp {
            video_time: now.video_time + now.video_refresh_period,
            ..now
        };
        (callback, guard.context, now, output)
    };
    // Called with the lock released: the guest's callback may stop the link, and
    // `CVDisplayLinkStop` takes the same lock.
    let mut flags_out = 0u64;
    // SAFETY: `callback` is the thunk the guest's function was registered
    // through, and the two timestamps outlive the call.
    unsafe { callback(info, &now, &output, 0, &mut flags_out, context) };
}

exports::exports! {
    functions {
        "CVDisplayLinkCreateWithCGDisplay" => CVDisplayLinkCreateWithCGDisplay,
        "CVDisplayLinkCreateWithActiveCGDisplays" => CVDisplayLinkCreateWithActiveCGDisplays,
        "CVDisplayLinkCreateWithCGDisplays" => CVDisplayLinkCreateWithCGDisplays,
        "CVGetHostClockFrequency" => CVGetHostClockFrequency,
        "CVPixelBufferCreateWithBytes" => CVPixelBufferCreateWithBytes,
        "CVPixelBufferLockBaseAddress" => CVPixelBufferLockBaseAddress,
        "CVPixelBufferUnlockBaseAddress" => CVPixelBufferUnlockBaseAddress,
        "CVPixelBufferRelease" => CVPixelBufferRelease,
        "CVBufferRetain" => CVBufferRetain,
        "CVBufferRelease" => CVBufferRelease,
        "CVDisplayLinkSetOutputCallback" => CVDisplayLinkSetOutputCallback,
        "CVDisplayLinkSetCurrentCGDisplay" => CVDisplayLinkSetCurrentCGDisplay,
        "CVDisplayLinkSetCurrentCGDisplayFromOpenGLContext" => CVDisplayLinkSetCurrentCGDisplayFromOpenGLContext,
        "CVDisplayLinkGetActualOutputVideoRefreshPeriod" => CVDisplayLinkGetActualOutputVideoRefreshPeriod,
        "CVDisplayLinkGetNominalOutputVideoRefreshPeriod" => CVDisplayLinkGetNominalOutputVideoRefreshPeriod,
        "CVDisplayLinkStart" => CVDisplayLinkStart,
        "CVDisplayLinkStop" => CVDisplayLinkStop,
        "CVDisplayLinkRetain" => CVDisplayLinkRetain,
        "CVDisplayLinkRelease" => CVDisplayLinkRelease,
        "CVDisplayLinkIsRunning" => CVDisplayLinkIsRunning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static FIRED: AtomicUsize = AtomicUsize::new(0);
    static SEEN_CONTEXT: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn record(
        _link: *mut c_void,
        now: *const TimeStamp,
        output: *const TimeStamp,
        _flags: u64,
        _flags_out: *mut u64,
        context: *mut c_void,
    ) -> i32 {
        FIRED.fetch_add(1, Ordering::SeqCst);
        SEEN_CONTEXT.store(context as usize, Ordering::SeqCst);
        // SAFETY: two timestamps the caller built on its own frame.
        unsafe {
            assert!((*output).video_time > (*now).video_time, "output is later");
        }
        SUCCESS
    }

    #[test]
    fn a_started_link_calls_back_on_its_own_thread() {
        FIRED.store(0, Ordering::SeqCst);
        let mut handle = std::ptr::null_mut();
        // SAFETY: a writable slot.
        assert_eq!(
            unsafe { CVDisplayLinkCreateWithCGDisplay(0, &mut handle) },
            SUCCESS
        );
        assert!(!handle.is_null());

        // SAFETY: `record` has the callback's prototype.
        assert_eq!(
            unsafe {
                CVDisplayLinkSetOutputCallback(handle, record as *mut c_void, 0x1234 as *mut c_void)
            },
            SUCCESS
        );
        // SAFETY: a link this test made.
        assert_eq!(unsafe { CVDisplayLinkStart(handle) }, SUCCESS);
        assert_eq!(unsafe { CVDisplayLinkIsRunning(handle) }, 1);

        // Nothing is turned here, which is the whole point: this thread does
        // not serve the link, so a guest may block on it.
        for _ in 0..100 {
            if FIRED.load(Ordering::SeqCst) >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            FIRED.load(Ordering::SeqCst) >= 2,
            "the link did not call back twice"
        );
        assert_eq!(
            SEEN_CONTEXT.load(Ordering::SeqCst),
            0x1234,
            "the context arrives"
        );

        // SAFETY: a link this test made.
        assert_eq!(unsafe { CVDisplayLinkStop(handle) }, SUCCESS);
        assert_eq!(unsafe { CVDisplayLinkIsRunning(handle) }, 0);

        // And a stopped link stops: the thread notices the flag within a frame.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let settled = FIRED.load(Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(
            FIRED.load(Ordering::SeqCst),
            settled,
            "the thread kept firing"
        );
    }

    /// Unity 5 caps its frame rate from the nominal period, and reads zero as
    /// an invalid rate.
    #[test]
    fn the_nominal_period_is_the_actual_one_as_a_time() {
        let mut handle = std::ptr::null_mut();
        // SAFETY: a writable slot, then the handle it was given.
        unsafe {
            assert_eq!(CVDisplayLinkCreateWithCGDisplay(0, &mut handle), SUCCESS);
            let actual = CVDisplayLinkGetActualOutputVideoRefreshPeriod(handle);
            let nominal = CVDisplayLinkGetNominalOutputVideoRefreshPeriod(handle);
            assert!(nominal.time_value > 0 && nominal.time_scale > 0);
            let seconds = nominal.time_value as f64 / f64::from(nominal.time_scale);
            assert!((seconds - actual).abs() < 1e-6);
            CVDisplayLinkRelease(handle);
            assert_eq!(
                CVDisplayLinkGetNominalOutputVideoRefreshPeriod(0x1 as *mut c_void).flags,
                TIME_IS_INDEFINITE
            );
        }
    }

    /// A handle nothing made is refused rather than dereferenced.
    #[test]
    fn an_unknown_handle_is_an_invalid_argument() {
        // SAFETY: a pointer this module never handed out.
        unsafe {
            assert_eq!(CVDisplayLinkStart(0x1 as *mut c_void), INVALID_ARGUMENT);
            assert_eq!(
                CVDisplayLinkSetOutputCallback(
                    0x1 as *mut c_void,
                    std::ptr::null_mut(),
                    std::ptr::null_mut()
                ),
                INVALID_ARGUMENT
            );
        }
    }

    /// `CVDisplayLinkCreateWithCGDisplays` reads the first display of the set
    /// and otherwise behaves as the single-display spelling.
    #[test]
    fn create_with_cg_displays_uses_the_first_display() {
        let displays = [0u32, 99];
        let mut handle = std::ptr::null_mut();
        // SAFETY: a two-element array and a writable slot.
        unsafe {
            assert_eq!(
                CVDisplayLinkCreateWithCGDisplays(displays.as_ptr(), 2, &mut handle),
                SUCCESS
            );
        }
        assert!(!handle.is_null());
        // SAFETY: `handle` was just created above.
        unsafe {
            assert!(CVDisplayLinkGetActualOutputVideoRefreshPeriod(handle) > 0.0);
            CVDisplayLinkRelease(handle);
        }
    }

    #[test]
    fn create_with_cg_displays_refuses_a_bad_count_or_null_array() {
        let mut handle = std::ptr::null_mut();
        // SAFETY: refused before either pointer is read.
        unsafe {
            assert_eq!(
                CVDisplayLinkCreateWithCGDisplays(std::ptr::null(), 1, &mut handle),
                INVALID_ARGUMENT
            );
            let displays = [0u32];
            assert_eq!(
                CVDisplayLinkCreateWithCGDisplays(displays.as_ptr(), 0, &mut handle),
                INVALID_ARGUMENT
            );
        }
    }

    /// Unity's frame cap and any other caller dividing by this expects a
    /// nanosecond tick, the same one `mach_absolute_time` reports.
    #[test]
    fn host_clock_frequency_is_a_nanosecond_tick() {
        // SAFETY: no arguments, nothing to read.
        assert_eq!(unsafe { CVGetHostClockFrequency() }, 1e9);
    }

    static RELEASED: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn record_release(ref_con: *mut c_void, base_address: *const c_void) {
        RELEASED.fetch_add(1, Ordering::SeqCst);
        assert_eq!(ref_con as usize, 0xf00d);
        assert_eq!(base_address as usize, 0xcafe);
    }

    /// Wraps the caller's bytes without copying, and releases them - once -
    /// when the last reference goes away.
    #[test]
    fn pixel_buffer_wraps_bytes_and_releases_once() {
        RELEASED.store(0, Ordering::SeqCst);
        let mut handle = std::ptr::null_mut();
        // SAFETY: a writable slot and a release callback matching the
        // declared prototype; `0xcafe` stands in for memory this test never
        // dereferences.
        unsafe {
            assert_eq!(
                CVPixelBufferCreateWithBytes(
                    std::ptr::null(),
                    4,
                    4,
                    0,
                    0xcafe as *mut c_void,
                    16,
                    Some(record_release),
                    0xf00d as *mut c_void,
                    std::ptr::null(),
                    &mut handle,
                ),
                SUCCESS
            );
        }
        assert!(!handle.is_null());

        // SAFETY: `handle` was just created above.
        unsafe {
            assert_eq!(CVPixelBufferLockBaseAddress(handle, 0), SUCCESS);
            assert_eq!(CVPixelBufferUnlockBaseAddress(handle, 0), SUCCESS);

            // A second owner, by `CVBufferRetain`, holds the buffer alive
            // past one `CVPixelBufferRelease`.
            let retained = CVBufferRetain(handle);
            assert_eq!(retained, handle);
            CVPixelBufferRelease(handle);
            assert_eq!(RELEASED.load(Ordering::SeqCst), 0, "one owner remains");
            assert_eq!(CVPixelBufferLockBaseAddress(handle, 0), SUCCESS);

            CVBufferRelease(handle);
            assert_eq!(
                RELEASED.load(Ordering::SeqCst),
                1,
                "the last owner released"
            );
            assert_eq!(
                CVPixelBufferLockBaseAddress(handle, 0),
                INVALID_ARGUMENT,
                "gone from the table"
            );
        }
    }

    /// `CVBufferRetain`/`CVBufferRelease` are null-safe, unlike `CFRetain`.
    #[test]
    fn buffer_retain_and_release_are_null_safe() {
        // SAFETY: null is the one input these two guarantee not to fault on.
        unsafe {
            assert_eq!(CVBufferRetain(std::ptr::null_mut()), std::ptr::null_mut());
            CVBufferRelease(std::ptr::null_mut());
        }
    }

    /// A buffer never created, as much as a handle `CVDisplayLink` never made.
    #[test]
    fn pixel_buffer_create_refuses_null_base_address_or_out_slot() {
        let mut handle = std::ptr::null_mut();
        // SAFETY: refused before either pointer is used.
        unsafe {
            assert_eq!(
                CVPixelBufferCreateWithBytes(
                    std::ptr::null(),
                    4,
                    4,
                    0,
                    std::ptr::null_mut(),
                    16,
                    None,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &mut handle,
                ),
                INVALID_ARGUMENT
            );
            assert_eq!(
                CVPixelBufferCreateWithBytes(
                    std::ptr::null(),
                    4,
                    4,
                    0,
                    0xcafe as *mut c_void,
                    16,
                    None,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                ),
                INVALID_ARGUMENT
            );
        }
    }
}
