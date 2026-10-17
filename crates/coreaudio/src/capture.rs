//! Recording through an output unit.
//!
//! A HAL output unit (`ahal`) records once a program enables IO on its input
//! scope: element 1 is then the microphone, the program installs
//! `kAudioOutputUnitProperty_SetInputCallback`, and when that callback says
//! frames have arrived it collects them with `AudioUnitRender` on bus 1. FMOD
//! records this way, and so does most code written against `AUHAL` rather than
//! against the HAL's `IOProc`s.
//!
//! What is recorded lives here rather than on the [`crate::Unit`], behind a
//! lock of its own. The guest's input callback is called from SDL's recording
//! thread and calls straight back into `AudioUnitRender`, and closing the unit
//! waits for that thread inside SDL; with the unit's own lock in the middle of
//! either, the two would wait on each other.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use crate::render::{AudioBufferList, Callback, MAX_BUFFERS, Proc, TimeStamp};
use crate::status::*;
use crate::stream::{Format, flags, sdl_spec};

/// The element a HAL output unit records on.
pub(crate) const INPUT_ELEMENT: u32 = 1;

/// `AudioUnitScope`.
mod scope {
    pub const INPUT: u32 = 1;
    pub const OUTPUT: u32 = 2;
}

/// `kAudioUnitProperty_StreamFormat`.
const STREAM_FORMAT: u32 = 8;
/// `kAudioOutputUnitProperty_CurrentDevice`.
const CURRENT_DEVICE: u32 = 2000;
/// `kAudioOutputUnitProperty_EnableIO`.
const ENABLE_IO: u32 = 2003;
/// `kAudioOutputUnitProperty_SetInputCallback`.
const SET_INPUT_CALLBACK: u32 = 2005;
/// `kAudioOutputUnitProperty_HasIO`, which a program reads to ask whether an
/// element can be enabled at all.
const HAS_IO: u32 = 2006;

/// One output unit's recording side.
struct Capture {
    /// The format the program asked to be handed: the output scope of element
    /// 1. SDL converts the microphone to it.
    format: Format,
    /// `kAudioOutputUnitProperty_SetInputCallback`, as a host-callable pointer.
    callback: Option<Callback>,
    /// SDL's recording stream between `AudioUnitInitialize` and
    /// `AudioUnitUninitialize`, null otherwise.
    stream: sdl3::AudioStream,
    /// The slice the input callback was last told about, interleaved, in
    /// [`Capture::format`]'s sample type.
    slice: Vec<u8>,
    /// Frames in [`Capture::slice`].
    frames: usize,
    /// Frames recorded so far, which is the `mSampleTime` the callback is told.
    recorded: u64,
    /// Buffers lent to a program that renders into a list with null `mData`,
    /// which asks the unit to provide them. Kept until the next render, which
    /// is as long as Apple's are good for.
    lent: Vec<Vec<u8>>,
}

// SAFETY: as `crate::Unit` - the stream is SDL's own handle, used only through
// `sdl3::audio`, and the callback is a host-callable pointer this runtime was
// handed. Both are behind the mutex in the table below.
unsafe impl Send for Capture {}

impl Capture {
    fn sample_bytes(&self) -> usize {
        (self.format.bits_per_channel / 8) as usize
    }

    /// Bytes in one interleaved frame of [`Capture::slice`].
    fn frame_bytes(&self) -> usize {
        self.sample_bytes() * self.format.channels_per_frame as usize
    }
}

/// Every output unit that has had input enabled, by unit handle.
static CAPTURES: Mutex<Vec<(usize, Arc<Mutex<Capture>>)>> = Mutex::new(Vec::new());

fn capture_for(handle: *mut c_void) -> Option<Arc<Mutex<Capture>>> {
    CAPTURES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(h, _)| *h == handle as usize)
        .map(|(_, c)| Arc::clone(c))
}

/// The format the microphone runs at, as the input scope of element 1 reports
/// it: interleaved float at the device's own rate and width.
fn device_format() -> Option<Format> {
    let (rate, channels, _) = crate::hal::input_shape()?;
    Some(Format {
        sample_rate: rate,
        format_flags: flags::IS_FLOAT | flags::IS_PACKED,
        bytes_per_packet: 4 * channels,
        bytes_per_frame: 4 * channels,
        channels_per_frame: channels,
        ..Format::default()
    })
}

/// What element 1 hands a program that has not set a format of its own:
/// the canonical non-interleaved float, at the microphone's rate and width.
fn default_client_format() -> Format {
    let (rate, channels, _) = crate::hal::input_shape().unwrap_or((48_000.0, 1, 512));
    Format {
        sample_rate: rate,
        channels_per_frame: channels,
        ..Format::default()
    }
}

fn register(handle: *mut c_void) -> Arc<Mutex<Capture>> {
    let mut captures = CAPTURES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, existing)) = captures.iter().find(|(h, _)| *h == handle as usize) {
        return Arc::clone(existing);
    }
    let capture = Arc::new(Mutex::new(Capture {
        format: default_client_format(),
        callback: None,
        stream: std::ptr::null_mut(),
        slice: Vec::new(),
        frames: 0,
        recorded: 0,
        lent: Vec::new(),
    }));
    captures.push((handle as usize, Arc::clone(&capture)));
    capture
}

/// The properties that configure recording on an output unit, or `None` for
/// any other property, which the caller handles as before.
///
/// # Safety
///
/// As [`crate::property::AudioUnitSetProperty`]'s.
pub(crate) unsafe fn set_property(
    handle: *mut c_void,
    unit: &mut crate::Unit,
    id: u32,
    scope: u32,
    element: u32,
    data: *const c_void,
    size: u32,
) -> Option<i32> {
    if unit.kind != crate::Kind::Output || data.is_null() {
        return None;
    }
    let word = || match size >= 4 {
        // SAFETY: the caller's `size` readable bytes, checked.
        true => Some(unsafe { *(data as *const u32) }),
        false => None,
    };
    match id {
        ENABLE_IO => {
            let Some(value) = word() else {
                return Some(INVALID_PROPERTY_VALUE);
            };
            let enabled = value != 0;
            let status = match scope {
                scope::INPUT => {
                    // A Mac without a microphone refuses, and a program falls
                    // back to playing only.
                    if enabled && crate::hal::input_shape().is_none() {
                        return Some(INVALID_PROPERTY_VALUE);
                    }
                    if !unit.stream.is_null() || capture_open(handle) {
                        return Some(CANNOT_DO_IN_CURRENT_CONTEXT);
                    }
                    unit.input_enabled = enabled;
                    if enabled {
                        register(handle);
                    }
                    SUCCESS
                }
                scope::OUTPUT => {
                    if !unit.stream.is_null() || capture_open(handle) {
                        return Some(CANNOT_DO_IN_CURRENT_CONTEXT);
                    }
                    unit.output_enabled = enabled;
                    SUCCESS
                }
                _ => INVALID_SCOPE,
            };
            Some(status)
        }
        CURRENT_DEVICE => {
            let Some(device) = word() else {
                return Some(INVALID_PROPERTY_VALUE);
            };
            let known = device == crate::hal::OUTPUT_DEVICE
                || (device == crate::hal::INPUT_DEVICE && crate::hal::input_shape().is_some());
            if !known {
                return Some(INVALID_PROPERTY_VALUE);
            }
            unit.device = device;
            Some(SUCCESS)
        }
        SET_INPUT_CALLBACK => {
            if (size as usize) < size_of::<[usize; 2]>() {
                return Some(INVALID_PROPERTY_VALUE);
            }
            // SAFETY: an `AURenderCallbackStruct`, whose function pointer the
            // shim already made host callable.
            let raw = unsafe { *(data as *const [*mut c_void; 2]) };
            let callback = (!raw[0].is_null()).then(|| Callback {
                // SAFETY: as above.
                proc: unsafe { std::mem::transmute::<*mut c_void, Proc>(raw[0]) },
                ref_con: raw[1],
            });
            register(handle)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .callback = callback;
            Some(SUCCESS)
        }
        STREAM_FORMAT if element == INPUT_ELEMENT => {
            if (size as usize) < size_of::<Format>() {
                return Some(INVALID_PROPERTY_VALUE);
            }
            // SAFETY: the caller's `size` readable bytes, checked.
            let format = unsafe { *(data as *const Format) };
            let status = match scope {
                // The microphone's side runs at whatever the host gives it, and
                // SDL converts; a program setting it is told yes, as Apple's
                // does for a format the device can take.
                scope::INPUT => match sdl_spec(&format) {
                    Some(_) => SUCCESS,
                    None => FORMAT_NOT_SUPPORTED,
                },
                scope::OUTPUT => {
                    if sdl_spec(&format).is_none()
                        || (format.non_interleaved()
                            && format.channels_per_frame as usize > MAX_BUFFERS)
                    {
                        return Some(FORMAT_NOT_SUPPORTED);
                    }
                    let capture = register(handle);
                    let mut capture = capture.lock().unwrap_or_else(|e| e.into_inner());
                    if !capture.stream.is_null() {
                        return Some(CANNOT_DO_IN_CURRENT_CONTEXT);
                    }
                    capture.format = format;
                    SUCCESS
                }
                _ => INVALID_SCOPE,
            };
            crate::trace::say(
                format_args!(
                    "input element format, scope {scope}: {} Hz, {} channels, {} bits, flags {:#x}",
                    format.sample_rate,
                    format.channels_per_frame,
                    format.bits_per_channel,
                    format.format_flags,
                ),
                status,
            );
            Some(status)
        }
        _ => None,
    }
}

/// The recording properties read back, or `None` for any other property.
///
/// # Safety
///
/// As [`crate::property::AudioUnitGetProperty`]'s.
pub(crate) unsafe fn get_property(
    handle: *mut c_void,
    unit: &crate::Unit,
    id: u32,
    scope: u32,
    element: u32,
    out: *mut c_void,
    size: *mut u32,
) -> Option<i32> {
    if unit.kind != crate::Kind::Output {
        return None;
    }
    let value: Vec<u8> = match id {
        ENABLE_IO => {
            let enabled = match scope {
                scope::INPUT => unit.input_enabled,
                scope::OUTPUT => unit.output_enabled,
                _ => return Some(INVALID_SCOPE),
            };
            u32::from(enabled).to_ne_bytes().to_vec()
        }
        HAS_IO => {
            let has = match scope {
                scope::INPUT => crate::hal::input_shape().is_some(),
                _ => true,
            };
            u32::from(has).to_ne_bytes().to_vec()
        }
        CURRENT_DEVICE => unit.device.to_ne_bytes().to_vec(),
        STREAM_FORMAT if element == INPUT_ELEMENT => {
            let format = match scope {
                scope::INPUT => match device_format() {
                    Some(format) => format,
                    None => return Some(INVALID_ELEMENT),
                },
                scope::OUTPUT => capture_for(handle).map_or_else(default_client_format, |c| {
                    c.lock().unwrap_or_else(|e| e.into_inner()).format
                }),
                _ => return Some(INVALID_SCOPE),
            };
            // SAFETY: `Format` is forty plain bytes.
            unsafe {
                std::slice::from_raw_parts(
                    (&format as *const Format).cast::<u8>(),
                    size_of::<Format>(),
                )
            }
            .to_vec()
        }
        _ => return None,
    };
    let wanted = value.len() as u32;
    // SAFETY: the caller's out-parameters; the capacity is checked before
    // anything is written.
    unsafe {
        let capacity = *size;
        *size = wanted;
        if out.is_null() {
            return Some(SUCCESS);
        }
        if capacity < wanted {
            return Some(INVALID_PROPERTY_VALUE);
        }
        std::ptr::copy_nonoverlapping(value.as_ptr(), out.cast::<u8>(), value.len());
    }
    Some(SUCCESS)
}

fn capture_open(handle: *mut c_void) -> bool {
    capture_for(handle)
        .is_some_and(|c| !c.lock().unwrap_or_else(|e| e.into_inner()).stream.is_null())
}

/// Open the microphone for `handle`, paused, as `AudioUnitInitialize` opens a
/// playback device. True when there is nothing to open.
pub(crate) fn open(handle: *mut c_void) -> bool {
    let capture = register(handle);
    let mut capture = capture.lock().unwrap_or_else(|e| e.into_inner());
    if !capture.stream.is_null() {
        return true;
    }
    let Some(spec) = sdl_spec(&capture.format) else {
        return false;
    };
    // SAFETY: `record` has SDL's callback prototype, and `handle` is a key
    // into a table rather than a borrow.
    let stream = unsafe { sdl3::audio::open_recording(spec, Some(record), handle) };
    let status = match stream {
        Some(stream) => {
            capture.stream = stream;
            capture.recorded = 0;
            SUCCESS
        }
        None => CANNOT_DO_IN_CURRENT_CONTEXT,
    };
    crate::trace::say(
        format_args!(
            "open microphone {} Hz, {} channels",
            spec.freq, spec.channels
        ),
        status,
    );
    status == SUCCESS
}

/// Resume or pause the microphone with the unit.
pub(crate) fn run(handle: *mut c_void, running: bool) {
    let Some(capture) = capture_for(handle) else {
        return;
    };
    let capture = capture.lock().unwrap_or_else(|e| e.into_inner());
    if capture.stream.is_null() {
        return;
    }
    // SAFETY: a stream this module opened.
    unsafe {
        match running {
            true => sdl3::audio::resume(capture.stream),
            false => sdl3::audio::pause(capture.stream),
        }
    };
}

/// Close the microphone, if `handle` has it open.
///
/// The caller must not hold the unit's lock: SDL's destroy waits for the
/// recording thread, which may be in the guest's callback, which may ask the
/// unit for something.
pub(crate) fn close(handle: *mut c_void) {
    let Some(capture) = capture_for(handle) else {
        return;
    };
    let stream = {
        let mut capture = capture.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::replace(&mut capture.stream, std::ptr::null_mut())
    };
    if !stream.is_null() {
        // SAFETY: the handle came from `open`, and nothing else holds it now.
        unsafe { sdl3::audio::destroy(stream) };
    }
}

/// Close the microphone and forget the unit, which is being disposed of.
pub(crate) fn forget(handle: *mut c_void) {
    close(handle);
    CAPTURES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(h, _)| *h != handle as usize);
}

/// SDL has recorded more: tell the guest about each whole slice of it.
///
/// # Safety
///
/// `userdata` is the unit handle [`open`] passed, and SDL calls this on its own
/// recording thread with the stream it opened.
unsafe extern "C" fn record(
    userdata: *mut c_void,
    stream: sdl3::AudioStream,
    _additional: i32,
    _total: i32,
) {
    let Some(capture) = capture_for(userdata) else {
        return;
    };
    // A guest sizes its own buffers against `kAudioUnitProperty_MaximumFramesPerSlice`,
    // so a slice is never longer than that.
    let per_slice = crate::hal::input_shape()
        .map_or(512, |(_, _, frames)| frames)
        .min(crate::render::FRAMES_PER_PULL_MAX)
        .max(1) as usize;
    let mut entered = None;
    loop {
        let (callback, frames, now) = {
            let mut capture = capture.lock().unwrap_or_else(|e| e.into_inner());
            if capture.stream.is_null() {
                return;
            }
            let bytes = per_slice * capture.frame_bytes();
            if bytes == 0 {
                return;
            }
            // SAFETY: SDL's stream, live for the duration of its own callback.
            if unsafe { sdl3::audio::available(stream) } < bytes {
                return;
            }
            capture.slice.resize(bytes, 0);
            // SAFETY: as above, and a live buffer.
            if unsafe { sdl3::audio::get(stream, &mut capture.slice) } < bytes {
                return;
            }
            capture.frames = per_slice;
            let now = TimeStamp {
                sample_time: capture.recorded as f64,
                host_time: crate::host_time(),
                rate_scalar: 1.0,
                // `kAudioTimeStampSampleTimeValid | kAudioTimeStampHostTimeValid`.
                flags: 1 | 2,
                ..TimeStamp::default()
            };
            capture.recorded += per_slice as u64;
            (capture.callback, per_slice, now)
        };
        // A unit recording with no callback installed drops what it records,
        // as Apple's does.
        let Some(callback) = callback else {
            continue;
        };
        // The callback is guest code, on SDL's thread, as in `render::feed`.
        entered.get_or_insert_with(darwin::tls::DarwinThread::enter);
        let mut action = 0u32;
        // SAFETY: a host-callable pointer this runtime was handed for the
        // guest's function, and every structure outlives the call. `ioData` is
        // null: the program renders bus 1 to collect the frames.
        unsafe {
            (callback.proc)(
                callback.ref_con,
                &mut action,
                &now,
                INPUT_ELEMENT,
                frames as u32,
                std::ptr::null_mut(),
            )
        };
    }
}

/// `AudioUnitRender` on bus 1 of a unit that records: copy the slice the input
/// callback was told about into the program's buffers. `None` when `handle`
/// does not record.
///
/// # Safety
///
/// `list` must be a writable `AudioBufferList` whose non-null buffers are
/// writable for their own `mDataByteSize`.
pub(crate) unsafe fn render(
    handle: *mut c_void,
    frames: u32,
    list: *mut AudioBufferList,
) -> Option<i32> {
    let capture = capture_for(handle)?;
    let mut capture = capture.lock().unwrap_or_else(|e| e.into_inner());
    if capture.stream.is_null() {
        return Some(CANNOT_DO_IN_CURRENT_CONTEXT);
    }
    let frames = frames as usize;
    let sample = capture.sample_bytes();
    let channels = capture.format.channels_per_frame as usize;
    let have = capture.frames.min(frames);
    // SAFETY: the caller's list.
    let list = unsafe { &mut *list };
    let buffers = match capture.format.non_interleaved() {
        true => channels,
        false => 1,
    };
    if (list.count as usize) < buffers {
        return Some(PARAM_ERROR);
    }
    let per_buffer = match buffers {
        1 => frames * sample * channels,
        _ => frames * sample,
    };
    let Capture { slice, lent, .. } = &mut *capture;
    lent.resize_with(buffers, Vec::new);
    for (index, buffer) in list.buffers.iter_mut().take(buffers).enumerate() {
        if buffer.data.is_null() {
            lent[index].resize(per_buffer, 0);
            buffer.data = lent[index].as_mut_ptr().cast();
            buffer.byte_size = per_buffer as u32;
        }
        let room = (buffer.byte_size as usize).min(per_buffer);
        // SAFETY: the caller's buffer, writable for its `mDataByteSize`, or
        // one lent above of `per_buffer` bytes.
        let out = unsafe { std::slice::from_raw_parts_mut(buffer.data.cast::<u8>(), room) };
        out.fill(0);
        match buffers {
            1 => {
                let bytes = (have * sample * channels).min(room);
                out[..bytes].copy_from_slice(&slice[..bytes]);
            }
            _ => {
                for frame in 0..have.min(room / sample) {
                    let from = (frame * channels + index) * sample;
                    out[frame * sample..(frame + 1) * sample]
                        .copy_from_slice(&slice[from..from + sample]);
                }
            }
        }
        buffer.channels = match buffers {
            1 => channels as u32,
            _ => 1,
        };
        buffer.byte_size = room as u32;
    }
    Some(SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::AudioBuffer;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    static FRAMES: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicU64 = AtomicU64::new(0);

    /// What FMOD's input callback does: render bus 1 into a buffer of its own.
    unsafe extern "C" fn collect(
        unit: *mut c_void,
        flags: *mut u32,
        now: *const TimeStamp,
        bus: u32,
        frames: u32,
        _data: *mut AudioBufferList,
    ) -> i32 {
        let mut samples = vec![0f32; frames as usize];
        let mut list = AudioBufferList {
            count: 1,
            buffers: [AudioBuffer {
                channels: 1,
                byte_size: frames * 4,
                data: std::ptr::null_mut(),
            }; MAX_BUFFERS],
        };
        list.buffers[0].data = samples.as_mut_ptr().cast();
        // SAFETY: a live list over a live buffer of the size declared.
        let status = unsafe { crate::AudioUnitRender(unit, flags, now, bus, frames, &mut list) };
        if status == SUCCESS {
            FRAMES.fetch_add(frames as usize, Ordering::Relaxed);
            let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
            PEAK.fetch_max(peak.to_bits() as u64, Ordering::Relaxed);
        }
        status
    }

    /// Records a second from the host's default microphone the way FMOD sets up
    /// an `ahal` unit for it. Needs a microphone, so it is run by hand:
    /// `cargo test -p coreaudio -- --ignored --nocapture records_from`.
    #[test]
    #[ignore]
    fn records_from_the_default_microphone_through_an_output_unit() {
        if crate::hal::input_shape().is_none() {
            eprintln!("no microphone; nothing to test");
            return;
        }
        let mut handle = std::ptr::null_mut();
        // SAFETY: live values of the sizes declared, on this crate's own unit.
        unsafe {
            assert_eq!(
                crate::AudioComponentInstanceNew(crate::test_component(), &mut handle),
                SUCCESS
            );
            let set = |id: u32, scope: u32, element: u32, data: *const c_void, size: usize| {
                crate::property::AudioUnitSetProperty(handle, id, scope, element, data, size as u32)
            };
            let on = 1u32;
            let off = 0u32;
            assert_eq!(
                set(ENABLE_IO, scope::INPUT, 1, (&on as *const u32).cast(), 4),
                SUCCESS
            );
            assert_eq!(
                set(ENABLE_IO, scope::OUTPUT, 0, (&off as *const u32).cast(), 4),
                SUCCESS
            );
            let device = crate::hal::INPUT_DEVICE;
            assert_eq!(
                set(CURRENT_DEVICE, 0, 0, (&device as *const u32).cast(), 4),
                SUCCESS
            );
            let mono = Format {
                sample_rate: 48_000.0,
                format_flags: flags::IS_FLOAT | flags::IS_PACKED,
                bytes_per_packet: 4,
                bytes_per_frame: 4,
                channels_per_frame: 1,
                ..Format::default()
            };
            assert_eq!(
                set(
                    STREAM_FORMAT,
                    scope::OUTPUT,
                    1,
                    (&mono as *const Format).cast(),
                    size_of::<Format>()
                ),
                SUCCESS
            );
            let callback: [*mut c_void; 2] = [collect as *mut c_void, handle];
            assert_eq!(
                set(SET_INPUT_CALLBACK, 0, 0, callback.as_ptr().cast(), 16),
                SUCCESS
            );
            assert_eq!(crate::AudioUnitInitialize(handle), SUCCESS);
            assert_eq!(crate::AudioOutputUnitStart(handle), SUCCESS);
            std::thread::sleep(std::time::Duration::from_secs(1));
            assert_eq!(crate::AudioOutputUnitStop(handle), SUCCESS);
            crate::AudioComponentInstanceDispose(handle);
        }
        let frames = FRAMES.load(Ordering::Relaxed);
        let peak = f32::from_bits(PEAK.load(Ordering::Relaxed) as u32);
        eprintln!("recorded {frames} frames, peak {peak}");
        assert!(frames >= 24_000, "only {frames} frames in a second");
    }
}
