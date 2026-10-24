//! `VTDecompressionSession`: H.264 samples in, pixel buffers out through the
//! output callback.
//!
//! Frames are decoded and delivered on the thread that submits them, even
//! when asynchronous decompression is asked for: that flag permits
//! asynchrony, it does not require it. A frame the decoder holds back to
//! reorder comes out of a later `DecodeFrame` or of `FinishDelayedFrames`.
//! No lock is held while the callback runs, and no thread is made, so guest
//! code only ever runs on threads the guest gave us.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::ThreadId;

use corefoundation::base::{CFTypeID, CFTypeRef};
use coremedia::Time;
use objc::class::Class;
use objc::object::Id;

use crate::backend::{self, Backend, DecodeError, Decoder, HwPolicy, Registry, StreamConfig};
use crate::bitstream::Converter;
use crate::output::{self, OutputSpec};

#[cfg(test)]
mod tests;

/// `OSStatus` values from `VTErrors.h`.
pub const PROPERTY_NOT_SUPPORTED: i32 = -12900;
pub const PARAMETER_ERR: i32 = crate::PARAMETER_ERR;
pub const INVALID_SESSION: i32 = -12903;
pub const ALLOCATION_FAILED: i32 = -12904;
pub const COULD_NOT_FIND_VIDEO_DECODER: i32 = crate::COULD_NOT_FIND_VIDEO_DECODER;
pub const BAD_DATA: i32 = -12909;
pub const MALFUNCTION: i32 = -12911;
pub const FORMAT_DESCRIPTION_CHANGE_NOT_SUPPORTED: i32 = -12916;

/// `kVTDecodeFrame_DoNotOutputFrame`. The other `VTDecodeFrameFlags`
/// (asynchronous, 1x real-time playback, temporal processing) change nothing
/// here: frames always come out in display order, on the caller's thread.
pub const DECODE_DO_NOT_OUTPUT_FRAME: u32 = 2;

/// `VTDecodeInfoFlags`; `kVTDecodeInfo_Asynchronous` is never reported.
pub const INFO_FRAME_DROPPED: u32 = 2;
pub const INFO_IMAGE_BUFFER_MODIFIABLE: u32 = 4;

/// More frames than H.264 can hold back (16) waiting for a picture means
/// the oldest were dropped by the decoder.
const MAX_PENDING: usize = 32;

const REAL_TIME: &str = "RealTime";
const USING_HARDWARE: &str = "UsingHardwareAcceleratedVideoDecoder";
const ENABLE_HARDWARE: &str = "EnableHardwareAcceleratedVideoDecoder";
const REQUIRE_HARDWARE: &str = "RequireHardwareAcceleratedVideoDecoder";
/// `kVTDecompressionPropertyKey_MaxNumberOfFramesBeforeOutput`: a cap on how
/// many frames the decoder may hold back for reordering. Accepted and
/// stored - every backend here already delivers in display order on the
/// submitting thread, so there is no reorder wait to cap - rather than
/// answered with `kVTPropertyNotSupportedErr`, which is what sent Roblox's
/// video down a path that never loaded.
const MAX_FRAMES_BEFORE_OUTPUT: &str = "MaxNumberOfFramesBeforeOutput";

/// The session property keys, as `(Mach-O name, text)`; the texts are Apple's.
pub(crate) const KEYS: &[(&str, &str)] = &[
    ("_kVTDecompressionPropertyKey_RealTime", REAL_TIME),
    (
        "_kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder",
        USING_HARDWARE,
    ),
    (
        "_kVTDecompressionPropertyKey_MaxNumberOfFramesBeforeOutput",
        MAX_FRAMES_BEFORE_OUTPUT,
    ),
];

/// `VTDecompressionOutputCallback`. The two `CMTime`s are passed in memory,
/// as any twenty-four byte struct is.
pub type OutputCallback = unsafe extern "C" fn(
    refcon: *mut c_void,
    source_frame_refcon: *mut c_void,
    status: i32,
    info_flags: u32,
    image_buffer: CFTypeRef,
    presentation_time_stamp: Time,
    duration: Time,
);

/// `VTDecompressionOutputCallbackRecord`.
#[repr(C)]
pub struct CallbackRecord {
    pub callback: Option<OutputCallback>,
    pub refcon: *mut c_void,
}

/// A frame submitted and not yet called back for.
struct Pending {
    source_refcon: usize,
    pts: Time,
    duration: Time,
    do_not_output: bool,
}

/// A callback to make once the session lock is let go.
struct Ready {
    source_refcon: usize,
    status: i32,
    info_flags: u32,
    /// A pixel buffer this list holds a reference to, or null.
    image: usize,
    pts: Time,
    duration: Time,
    /// The picture's size and output format, for the first-frame diagnostic;
    /// `None` for a failed frame, which has no image to describe.
    frame: Option<(usize, usize, u32)>,
}

impl Ready {
    fn failed(pending: Pending, status: i32) -> Ready {
        Ready {
            source_refcon: pending.source_refcon,
            status,
            info_flags: INFO_FRAME_DROPPED,
            image: 0,
            pts: pending.pts,
            duration: pending.duration,
            frame: None,
        }
    }
}

struct Inner {
    decoder: Option<Box<dyn Decoder>>,
    converter: Converter,
    output: OutputSpec,
    pending: BTreeMap<u64, Pending>,
    next_token: u64,
    invalid: bool,
    malfunctioned: bool,
    /// The retained `RealTime` value, or null.
    real_time: usize,
    /// The retained `MaxNumberOfFramesBeforeOutput` value, or null.
    max_frames_before_output: usize,
}

struct State {
    inner: Mutex<Inner>,
    backend: &'static dyn Backend,
    /// The retained format description.
    format: usize,
    /// Its NAL unit length size and parameter sets.
    stream: (u8, Vec<Vec<u8>>),
    callback: OutputCallback,
    refcon: usize,
    /// The threads delivering callbacks now, once per delivery.
    delivering: Mutex<Vec<ThreadId>>,
    idle: Condvar,
    /// Whether a decoded frame has reached the guest's callback yet, for the
    /// one-line diagnostic that says video is actually flowing.
    delivered_first_frame: AtomicBool,
}

impl Drop for State {
    fn drop(&mut self) {
        release(self.format as CFTypeRef);
        let inner = self.inner.get_mut().unwrap_or_else(|e| e.into_inner());
        release(inner.real_time as CFTypeRef);
        release(inner.max_frames_before_output as CFTypeRef);
    }
}

#[repr(C)]
struct Session {
    isa: *const c_void,
    state: State,
}

fn class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let p = *CLASS.get_or_init(|| {
        corefoundation::bootstrap();
        let class = corefoundation::base::register(
            c"VTDecompressionSession",
            None,
            size_of::<Session>(),
            true,
        );
        class.replace_method(
            objc::sel::get("dealloc"),
            dealloc as *const c_void,
            c"v16@0:8",
        );
        objc::class::ptr(class) as usize
    });
    // SAFETY: a leaked class registered above.
    unsafe { &*(p as *const Class) }
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: objc::sel::Sel) {
    // SAFETY: `-dealloc` of a `VTDecompressionSession`, whose body is `Session`.
    let session = unsafe { &mut *(receiver as *mut Session) };
    invalidate(&session.state);
    // SAFETY: written once when the session was made, dropped only here.
    unsafe { std::ptr::drop_in_place(&mut session.state) };
    objc::arc::finalize(receiver);
}

fn state<'a>(session: CFTypeRef) -> Option<&'a State> {
    // SAFETY: null or an object a guest received from this runtime.
    let is_session =
        !session.is_null() && unsafe { corefoundation::base::is_kind_of(session, class()) };
    // SAFETY: checked to be a `VTDecompressionSession`, whose body is `Session`.
    is_session.then(|| unsafe { &(*(session as *const Session)).state })
}

fn release(object: CFTypeRef) {
    if !object.is_null() {
        // SAFETY: a reference the caller owns.
        unsafe { corefoundation::api::CFRelease(object) };
    }
}

fn retain(object: CFTypeRef) -> CFTypeRef {
    match object.is_null() {
        true => object,
        // SAFETY: a live CF object.
        false => unsafe { corefoundation::api::CFRetain(object) },
    }
}

fn key(text: &str) -> CFTypeRef {
    corefoundation::base::interned_name(text) as usize as CFTypeRef
}

fn is_key(candidate: CFTypeRef, text: &str) -> bool {
    // SAFETY: the caller's key, and a string made here.
    !candidate.is_null() && unsafe { corefoundation::api::CFEqual(candidate, key(text)) } != 0
}

fn lock(state: &State) -> MutexGuard<'_, Inner> {
    state.inner.lock().unwrap_or_else(|e| e.into_inner())
}

/// A boolean in a dictionary, if it is there.
fn flag(dictionary: CFTypeRef, name: &str) -> Option<bool> {
    if dictionary.is_null() {
        return None;
    }
    // SAFETY: the caller's dictionary and a key made here.
    let value = unsafe { corefoundation::api::CFDictionaryGetValue(dictionary, key(name)) };
    if value.is_null() {
        return None;
    }
    // SAFETY: a CF object from the dictionary.
    unsafe { corefoundation::collections::number(value) }.map(|n| n.as_i64() != 0)
}

/// Which backends a decoder specification allows: Apple's meaning of the two
/// keys, with every backend when neither is there.
fn policy(specification: CFTypeRef) -> HwPolicy {
    let mut policy = HwPolicy::ANY;
    if flag(specification, REQUIRE_HARDWARE) == Some(true) {
        policy.software = false;
    }
    if flag(specification, ENABLE_HARDWARE) == Some(false) {
        policy.hardware = false;
    }
    policy
}

/// The `avcC` record's NAL unit length size and its SPSs and PPSs, in that
/// order, or `None` for a description without one.
fn parameter_sets(format: CFTypeRef) -> Option<(u8, Vec<Vec<u8>>)> {
    let (mut count, mut length_size) = (0usize, 0i32);
    // SAFETY: the description, and out-parameters for the count and length.
    let status = unsafe {
        coremedia::CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            format,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut count,
            &mut length_size,
        )
    };
    if status != 0 || count == 0 {
        return None;
    }
    let mut sets = Vec::with_capacity(count);
    for index in 0..count {
        let (mut pointer, mut size) = (std::ptr::null(), 0usize);
        // SAFETY: as above, for one set; the pointer is into the description.
        let status = unsafe {
            coremedia::CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format,
                index,
                &mut pointer,
                &mut size,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != 0 || pointer.is_null() {
            return None;
        }
        // SAFETY: `size` bytes the description holds.
        sets.push(unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec());
    }
    // The decoder needs the SPSs and PPSs; SPS extensions are for layers it
    // does not decode.
    sets.retain(|set| {
        matches!(
            coremedia::h264::nal_type(set),
            Some(coremedia::h264::NAL_SPS | coremedia::h264::NAL_PPS)
        )
    });
    sets.sort_by_key(|set| coremedia::h264::nal_type(set) != Some(coremedia::h264::NAL_SPS));
    Some((u8::try_from(length_size).ok()?, sets))
}

/// `VTDecompressionSessionCreate`, after `lib.rs` has checked that there is
/// an out-parameter and a video description.
///
/// # Safety
///
/// The arguments are the guest's: `out` writable, the rest null or what
/// their types say.
pub(crate) unsafe fn create(
    format: CFTypeRef,
    specification: CFTypeRef,
    attributes: CFTypeRef,
    callback: *const CallbackRecord,
    out: *mut CFTypeRef,
) -> i32 {
    // SAFETY: as the caller promises.
    unsafe {
        create_with(
            Registry::global(),
            format,
            specification,
            attributes,
            callback,
            out,
        )
    }
}

/// [`create`] from the backends of `registry`.
///
/// # Safety
///
/// As [`create`].
pub(crate) unsafe fn create_with(
    registry: &Registry,
    format: CFTypeRef,
    specification: CFTypeRef,
    attributes: CFTypeRef,
    callback: *const CallbackRecord,
    out: *mut CFTypeRef,
) -> i32 {
    // SAFETY: the caller's out-parameter.
    unsafe { out.write(std::ptr::null()) };
    // SAFETY: a description the caller checked.
    let codec = unsafe { coremedia::CMFormatDescriptionGetMediaSubType(format) };
    if codec != coremedia::CODEC_H264 {
        backend::report_once(format!(
            "rosemary: VideoToolbox: no decoder for {}",
            corevideo::format::name(codec)
        ));
        return COULD_NOT_FIND_VIDEO_DECODER;
    }
    let Some((length_size, sets)) = parameter_sets(format) else {
        backend::report_once(
            "rosemary: VideoToolbox: an H.264 description without an avcC record cannot be decoded"
                .to_string(),
        );
        return COULD_NOT_FIND_VIDEO_DECODER;
    };
    // SAFETY: null or the caller's record.
    let record = unsafe { callback.as_ref() };
    let Some((Some(function), refcon)) = record.map(|r| (r.callback, r.refcon)) else {
        return PARAMETER_ERR;
    };
    let Some(sps) = sets.first().and_then(|s| coremedia::h264::parse_sps(s)) else {
        backend::report_once("rosemary: VideoToolbox: the H.264 SPS does not parse".to_string());
        return COULD_NOT_FIND_VIDEO_DECODER;
    };
    let output = match OutputSpec::parse(attributes, sps.width() as usize, sps.height() as usize) {
        Ok(output) => output,
        Err(why) => {
            backend::report_once(format!("rosemary: VideoToolbox: {why}"));
            return COULD_NOT_FIND_VIDEO_DECODER;
        }
    };
    let config = StreamConfig {
        sps: &sps,
        parameter_sets: &sets,
    };
    let (decoder, chosen) = match registry.open(&config, policy(specification)) {
        Ok(opened) => opened,
        Err(why) => {
            backend::report_once(format!("rosemary: VideoToolbox: no H.264 decoder: {why}"));
            return COULD_NOT_FIND_VIDEO_DECODER;
        }
    };
    backend::report_once(format!(
        "rosemary: VideoToolbox: H.264 through {}",
        chosen.description()
    ));
    let state = State {
        inner: Mutex::new(Inner {
            decoder: Some(decoder),
            converter: Converter::new(length_size, sets.clone()),
            output,
            pending: BTreeMap::new(),
            next_token: 0,
            invalid: false,
            malfunctioned: false,
            real_time: 0,
            max_frames_before_output: 0,
        }),
        backend: chosen,
        format: retain(format) as usize,
        stream: (length_size, sets),
        callback: function,
        refcon: refcon as usize,
        delivering: Mutex::new(Vec::new()),
        idle: Condvar::new(),
        delivered_first_frame: AtomicBool::new(false),
    };
    // SAFETY: `Session` is the registered layout.
    let session = unsafe { corefoundation::base::allocate::<Session>(class()) };
    // SAFETY: a fresh, zeroed allocation whose state is written once here.
    unsafe { std::ptr::write(&mut (*session).state, state) };
    // SAFETY: the caller's out-parameter.
    unsafe { out.write(session as CFTypeRef) };
    0
}

/// Whether `format` describes the session's stream: an equal description, or
/// one with the same picture size and the same `avcC` parameter sets. The
/// second does not depend on CF value equality, which needs Foundation's
/// `-isEqual:` to compare the extensions' data rather than their identity.
fn same_stream(state: &State, format: CFTypeRef) -> bool {
    let own = state.format as CFTypeRef;
    if format.is_null() {
        return false;
    }
    // SAFETY: two descriptions, or anything a guest passed.
    unsafe {
        if format == own || coremedia::CMFormatDescriptionEqual(format, own) != 0 {
            return true;
        }
        coremedia::CMFormatDescriptionGetMediaSubType(format) == coremedia::CODEC_H264
            && coremedia::CMVideoFormatDescriptionGetDimensions(format)
                == coremedia::CMVideoFormatDescriptionGetDimensions(own)
            && parameter_sets(format).as_ref() == Some(&state.stream)
    }
}

/// Keeps a session alive while a call uses it, whatever its callbacks do.
struct Hold(CFTypeRef);

impl Hold {
    fn new(session: CFTypeRef) -> Hold {
        Hold(retain(session))
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        release(self.0);
    }
}

/// Collects what the decoder gives into callbacks to make.
struct Collector<'a> {
    pending: &'a mut BTreeMap<u64, Pending>,
    output: &'a mut OutputSpec,
    ready: &'a mut Vec<Ready>,
}

impl Collector<'_> {
    fn take(&mut self, picture: crate::backend::Picture<'_>) {
        let Some(pending) = self.pending.remove(&picture.token) else {
            // A picture for an access unit already failed; nobody waits.
            return;
        };
        if pending.do_not_output {
            self.ready.push(Ready::failed(pending, 0));
            return;
        }
        // Resolved ahead of `make_image`, which does the same internally
        // (idempotently: the format is fixed by the first picture) - this
        // is the only way to learn it without reading it back off the
        // pixel buffer, which has no public accessor for it here.
        let format = self.output.resolve(&picture.colour);
        match output::make_image(&picture, self.output) {
            Ok(image) => self.ready.push(Ready {
                source_refcon: pending.source_refcon,
                status: 0,
                info_flags: INFO_IMAGE_BUFFER_MODIFIABLE,
                image: image as usize,
                pts: pending.pts,
                duration: pending.duration,
                frame: Some((picture.width, picture.height, format)),
            }),
            Err(status) => {
                backend::report_once(format!(
                    "rosemary: VideoToolbox: a {}x{} picture could not be made (CVReturn {status})",
                    picture.width, picture.height
                ));
                self.ready.push(Ready::failed(pending, ALLOCATION_FAILED));
            }
        }
    }
}

impl Inner {
    /// Stop decoding for good after the decoder broke. Every frame still
    /// waiting is failed, except `current`, which the caller reports.
    fn malfunction(
        &mut self,
        backend: &'static dyn Backend,
        global: bool,
        current: Option<u64>,
        ready: &mut Vec<Ready>,
    ) {
        self.malfunctioned = true;
        self.decoder = None;
        if global {
            backend.set_unhealthy();
            backend::report_once(format!(
                "rosemary: VideoToolbox: the {} decoder failed; it is not used again in this process",
                backend.name()
            ));
        }
        if let Some(token) = current {
            self.pending.remove(&token);
        }
        while let Some((_, pending)) = self.pending.pop_first() {
            ready.push(Ready::failed(pending, MALFUNCTION));
        }
    }

    /// Take what the decoder has ready. `true` if it broke.
    fn receive(
        &mut self,
        backend: &'static dyn Backend,
        drain: bool,
        current: Option<u64>,
        ready: &mut Vec<Ready>,
    ) -> bool {
        let Some(decoder) = self.decoder.as_mut() else {
            return false;
        };
        let mut collector = Collector {
            pending: &mut self.pending,
            output: &mut self.output,
            ready: &mut *ready,
        };
        let mut sink = |picture: crate::backend::Picture<'_>| collector.take(picture);
        let result = match drain {
            true => decoder.drain(&mut sink),
            false => decoder.receive(&mut sink),
        };
        match result {
            // A broken picture: the frames it held up are failed by the
            // pending limit or the next flush.
            Ok(()) | Err(DecodeError::BadData) => false,
            Err(DecodeError::Malfunction { global }) => {
                self.malfunction(backend, global, current, ready);
                true
            }
            Err(DecodeError::UnsupportedOutput) => {
                backend::report_once(format!(
                    "rosemary: VideoToolbox: the {} decoder made a picture in a form that cannot be output",
                    backend.name()
                ));
                self.malfunction(backend, false, current, ready);
                true
            }
        }
    }
}

/// Make the callbacks, then let go of their images.
fn deliver(state: &State, ready: Vec<Ready>) {
    if ready.is_empty() {
        return;
    }
    let me = std::thread::current().id();
    state
        .delivering
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(me);
    struct Done<'a>(&'a State, ThreadId);
    impl Drop for Done<'_> {
        fn drop(&mut self) {
            let mut delivering = self.0.delivering.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(at) = delivering.iter().position(|t| *t == self.1) {
                delivering.swap_remove(at);
            }
            if delivering.is_empty() {
                self.0.idle.notify_all();
            }
        }
    }
    let _done = Done(state, me);
    for frame in ready {
        if let Some((width, height, format)) = frame.frame {
            if !state.delivered_first_frame.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "rosemary: VideoToolbox: first frame delivered ({width}x{height}, format {})",
                    corevideo::format::name(format)
                );
            }
        }
        // Checked before each callback: one may invalidate the session.
        if !lock(state).invalid {
            // SAFETY: the guest's callback, with the arguments it registered
            // for and a buffer that is live until it returns.
            unsafe {
                (state.callback)(
                    state.refcon as *mut c_void,
                    frame.source_refcon as *mut c_void,
                    frame.status,
                    frame.info_flags,
                    frame.image as CFTypeRef,
                    frame.pts,
                    frame.duration,
                )
            };
        }
        release(frame.image as CFTypeRef);
    }
}

/// Wait until no callback is running, unless this thread is in one.
fn wait_idle(state: &State) {
    let me = std::thread::current().id();
    let mut delivering = state.delivering.lock().unwrap_or_else(|e| e.into_inner());
    while !delivering.is_empty() && !delivering.contains(&me) {
        delivering = state
            .idle
            .wait(delivering)
            .unwrap_or_else(|e| e.into_inner());
    }
}

fn invalidate(state: &State) {
    let (decoder, pending) = {
        let mut inner = lock(state);
        inner.invalid = true;
        (inner.decoder.take(), std::mem::take(&mut inner.pending))
    };
    drop(decoder);
    drop(pending);
    wait_idle(state);
}

/// `OSStatus VTDecompressionSessionDecodeFrame(VTDecompressionSessionRef,
/// CMSampleBufferRef, VTDecodeFrameFlags, void *sourceFrameRefCon,
/// VTDecodeInfoFlags *infoFlagsOut)`.
///
/// Decoded synchronously: `*infoFlagsOut` never has `Asynchronous`, and the
/// callbacks for every frame that is ready have run by the time this returns.
pub unsafe extern "C" fn VTDecompressionSessionDecodeFrame(
    session: CFTypeRef,
    sample: CFTypeRef,
    flags: u32,
    source_frame_refcon: *mut c_void,
    info_flags_out: *mut u32,
) -> i32 {
    if !info_flags_out.is_null() {
        // SAFETY: the caller's out-parameter.
        unsafe { info_flags_out.write(0) };
    }
    let Some(state) = state(session) else {
        return PARAMETER_ERR;
    };
    let _hold = Hold::new(session);
    let mut ready = Vec::new();
    let status = {
        let mut inner = lock(state);
        submit(
            state,
            &mut inner,
            sample,
            flags,
            source_frame_refcon,
            &mut ready,
        )
    };
    deliver(state, ready);
    status
}

fn submit(
    state: &State,
    inner: &mut Inner,
    sample: CFTypeRef,
    flags: u32,
    source_frame_refcon: *mut c_void,
    ready: &mut Vec<Ready>,
) -> i32 {
    if inner.invalid {
        return INVALID_SESSION;
    }
    if inner.malfunctioned {
        return MALFUNCTION;
    }
    // SAFETY: null or an object a guest received from this runtime.
    let usable = coremedia::sample::is_sample_buffer(sample)
        && unsafe {
            coremedia::sample::CMSampleBufferDataIsReady(sample) != 0
                && coremedia::sample::CMSampleBufferGetNumSamples(sample) == 1
        };
    let data = coremedia::sample::data(sample);
    let bytes = match usable {
        true => coremedia::block::bytes(data),
        false => None,
    };
    let Some(bytes) = bytes else {
        return BAD_DATA;
    };
    if !same_stream(state, coremedia::sample::format(sample)) {
        return FORMAT_DESCRIPTION_CHANGE_NOT_SUPPORTED;
    }
    // Copied here: the caller may free the sample once this returns.
    let Ok(unit) = inner.converter.convert(bytes) else {
        return BAD_DATA;
    };
    let token = inner.next_token;
    inner.next_token += 1;
    let (pts, duration) = coremedia::sample::timing(sample);
    inner.pending.insert(
        token,
        Pending {
            source_refcon: source_frame_refcon as usize,
            pts,
            duration,
            do_not_output: flags & DECODE_DO_NOT_OUTPUT_FRAME != 0,
        },
    );
    let Some(decoder) = inner.decoder.as_mut() else {
        inner.pending.remove(&token);
        return MALFUNCTION;
    };
    match decoder.send(&unit, token) {
        Ok(()) => {}
        Err(DecodeError::BadData) => {
            if let Some(pending) = inner.pending.remove(&token) {
                ready.push(Ready::failed(pending, BAD_DATA));
            }
        }
        Err(DecodeError::Malfunction { global }) => {
            inner.malfunction(state.backend, global, Some(token), ready);
            return MALFUNCTION;
        }
        Err(DecodeError::UnsupportedOutput) => {
            inner.malfunction(state.backend, false, Some(token), ready);
            return MALFUNCTION;
        }
    }
    if inner.receive(state.backend, false, Some(token), ready) {
        // The decoder broke while this frame was in it: it is failed by this
        // call's status rather than a callback, unless it came out already.
        return MALFUNCTION;
    }
    while inner.pending.len() > MAX_PENDING {
        if let Some((_, pending)) = inner.pending.pop_first() {
            ready.push(Ready::failed(pending, BAD_DATA));
        }
    }
    0
}

/// `OSStatus VTDecompressionSessionFinishDelayedFrames(VTDecompressionSessionRef)`.
///
/// FIXME (ACCURACY): FFmpeg cannot emit what it holds without forgetting its
/// reference pictures too, so decoding resumes cleanly only at the next IDR.
/// Apple's decoder carries on from where it was.
pub unsafe extern "C" fn VTDecompressionSessionFinishDelayedFrames(session: CFTypeRef) -> i32 {
    let Some(state) = state(session) else {
        return PARAMETER_ERR;
    };
    let _hold = Hold::new(session);
    let mut ready = Vec::new();
    {
        let mut inner = lock(state);
        if inner.invalid {
            return INVALID_SESSION;
        }
        inner.receive(state.backend, true, None, &mut ready);
        while let Some((_, pending)) = inner.pending.pop_first() {
            ready.push(Ready::failed(pending, BAD_DATA));
        }
        inner.converter.flushed();
    }
    deliver(state, ready);
    0
}

/// `OSStatus VTDecompressionSessionWaitForAsynchronousFrames(VTDecompressionSessionRef)`:
/// frames are delivered as they are submitted, so only callbacks running
/// on other threads are waited for.
pub unsafe extern "C" fn VTDecompressionSessionWaitForAsynchronousFrames(
    session: CFTypeRef,
) -> i32 {
    let Some(state) = state(session) else {
        return PARAMETER_ERR;
    };
    if lock(state).invalid {
        return INVALID_SESSION;
    }
    wait_idle(state);
    0
}

/// `void VTDecompressionSessionInvalidate(VTDecompressionSessionRef)`: no
/// callback runs once this returns, and frames still held are dropped
/// without one.
pub unsafe extern "C" fn VTDecompressionSessionInvalidate(session: CFTypeRef) {
    if let Some(state) = state(session) {
        invalidate(state);
    }
}

/// `Boolean VTDecompressionSessionCanAcceptFormatDescription(
/// VTDecompressionSessionRef, CMFormatDescriptionRef)`: only the session's own
/// description, which is the conservative answer Apple allows.
pub unsafe extern "C" fn VTDecompressionSessionCanAcceptFormatDescription(
    session: CFTypeRef,
    format: CFTypeRef,
) -> u8 {
    let Some(state) = state(session) else {
        return 0;
    };
    if lock(state).invalid {
        return 0;
    }
    u8::from(same_stream(state, format))
}

/// `CFTypeID VTDecompressionSessionGetTypeID(void)`.
pub unsafe extern "C" fn VTDecompressionSessionGetTypeID() -> CFTypeID {
    corefoundation::base::type_id(class())
}

/// `OSStatus VTSessionCopyProperty(VTSessionRef, CFStringRef key,
/// CFAllocatorRef, void *propertyValueOut)`.
pub unsafe extern "C" fn VTSessionCopyProperty(
    session: CFTypeRef,
    key: CFTypeRef,
    _allocator: CFTypeRef,
    out: *mut CFTypeRef,
) -> i32 {
    let Some(state) = state(session) else {
        return PARAMETER_ERR;
    };
    if out.is_null() || key.is_null() {
        return PARAMETER_ERR;
    }
    let inner = lock(state);
    if inner.invalid {
        return INVALID_SESSION;
    }
    let value = if is_key(key, USING_HARDWARE) {
        corefoundation::collections::boolean(state.backend.is_hardware())
    } else if is_key(key, REAL_TIME) {
        match inner.real_time {
            0 => corefoundation::collections::boolean(false),
            value => value as CFTypeRef,
        }
    } else if is_key(key, MAX_FRAMES_BEFORE_OUTPUT) {
        match inner.max_frames_before_output {
            0 => return PROPERTY_NOT_SUPPORTED,
            value => value as CFTypeRef,
        }
    } else {
        return PROPERTY_NOT_SUPPORTED;
    };
    // SAFETY: the caller's out-parameter, given a reference of its own.
    unsafe { out.write(retain(value)) };
    0
}

/// `OSStatus VTSessionSetProperty(VTSessionRef, CFStringRef key, CFTypeRef value)`.
pub unsafe extern "C" fn VTSessionSetProperty(
    session: CFTypeRef,
    key: CFTypeRef,
    value: CFTypeRef,
) -> i32 {
    let Some(state) = state(session) else {
        return PARAMETER_ERR;
    };
    let mut inner = lock(state);
    if inner.invalid {
        return INVALID_SESSION;
    }
    let slot = if is_key(key, REAL_TIME) {
        &mut inner.real_time
    } else if is_key(key, MAX_FRAMES_BEFORE_OUTPUT) {
        // A hint this runtime does not need to act on: every backend
        // already delivers frames in display order as they are submitted,
        // so there is no reorder wait for a cap to shorten. Stored, and
        // answered `noErr`, rather than `kVTPropertyNotSupportedErr` - which
        // is what a guest that treats that error as fatal reads as "this
        // decoder cannot be configured" and gives up on before ever
        // submitting a frame.
        &mut inner.max_frames_before_output
    } else {
        return PROPERTY_NOT_SUPPORTED;
    };
    let old = std::mem::replace(slot, retain(value) as usize);
    drop(inner);
    release(old as CFTypeRef);
    0
}

exports::exports! {
    functions {
        "VTDecompressionSessionDecodeFrame" => VTDecompressionSessionDecodeFrame,
        "VTDecompressionSessionFinishDelayedFrames" => VTDecompressionSessionFinishDelayedFrames,
        "VTDecompressionSessionWaitForAsynchronousFrames" =>
            VTDecompressionSessionWaitForAsynchronousFrames,
        "VTDecompressionSessionInvalidate" => VTDecompressionSessionInvalidate,
        "VTDecompressionSessionCanAcceptFormatDescription" =>
            VTDecompressionSessionCanAcceptFormatDescription,
        "VTDecompressionSessionGetTypeID" => VTDecompressionSessionGetTypeID,
        "VTSessionCopyProperty" => VTSessionCopyProperty,
        "VTSessionSetProperty" => VTSessionSetProperty,
    }
}
