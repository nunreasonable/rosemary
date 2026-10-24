use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use corefoundation::api::CFRelease;
use corefoundation::callbacks::Policy;
use corefoundation::collections::{Number, new_dictionary, new_number};
use coremedia::sample::TimingInfo;
use coremedia::time::CMTimeMake;
use corevideo::format::{BGRA, NV12_FULL, NV12_VIDEO, Y420};
use corevideo::pixelbuffer as pb;

use super::*;
use crate::backend::mock::{self, MockBackend};
use crate::backend::{Colour, PROBE_CLIP, PROBE_DISPLAY_ORDER};

/// `kVTDecodeFrame_EnableAsynchronousDecompression` and
/// `kVTDecodeFrame_EnableTemporalProcessing`.
const DECODE_ASYNCHRONOUS: u32 = 1;
const DECODE_TEMPORAL_PROCESSING: u32 = 8;

const NS: i32 = 1_000_000_000;
const FRAME_NS: i64 = 33_333_333;

/// One callback, with a reference of its own to the image.
#[derive(Debug)]
struct Frame {
    source: usize,
    status: i32,
    flags: u32,
    image: usize,
    pts: Time,
    duration: Time,
}

#[derive(Default)]
struct Recorder {
    frames: Mutex<Vec<Frame>>,
    session: AtomicUsize,
    invalidate_in_callback: AtomicBool,
}

impl Recorder {
    fn take(&self) -> Vec<Frame> {
        std::mem::take(&mut *self.frames.lock().unwrap())
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        release(self.image as CFTypeRef);
    }
}

unsafe extern "C" fn record(
    refcon: *mut c_void,
    source: *mut c_void,
    status: i32,
    flags: u32,
    image: CFTypeRef,
    pts: Time,
    duration: Time,
) {
    // SAFETY: the recorder the test registered, alive for the session.
    let recorder = unsafe { &*(refcon as *const Recorder) };
    if recorder.invalidate_in_callback.load(Ordering::Relaxed) {
        let session = recorder.session.load(Ordering::Relaxed) as CFTypeRef;
        // SAFETY: the session this callback is for.
        unsafe {
            VTDecompressionSessionInvalidate(session);
            assert_eq!(
                VTDecompressionSessionWaitForAsynchronousFrames(session),
                INVALID_SESSION
            );
        }
    }
    recorder.frames.lock().unwrap().push(Frame {
        source: source as usize,
        status,
        flags,
        image: retain(image) as usize,
        pts,
        duration,
    });
}

fn key_number(pairs: &[(&str, CFTypeRef)]) -> CFTypeRef {
    let entries: Vec<(CFTypeRef, CFTypeRef)> = pairs.iter().map(|(k, v)| (key(k), *v)).collect();
    new_dictionary(
        &entries,
        Policy::from_key_callbacks(),
        Policy::from_type_callbacks(),
        false,
    )
}

fn number(value: i64) -> CFTypeRef {
    new_number(Number::Integer(value))
}

fn empty_dictionary() -> CFTypeRef {
    key_number(&[])
}

/// `{PixelFormatType: format}`, with `IOSurfaceProperties: {}` if asked.
fn attributes(format: u32, iosurface: bool) -> CFTypeRef {
    let mut pairs = vec![(pb::PIXEL_FORMAT_TYPE_KEY, number(i64::from(format)))];
    if iosurface {
        pairs.push((pb::IOSURFACE_PROPERTIES_KEY, empty_dictionary()));
    }
    key_number(&pairs)
}

/// The probe clip's SPS, PPS and six slices.
fn probe_units() -> (Vec<u8>, Vec<u8>, Vec<Vec<u8>>) {
    let units = crate::bitstream::split_annex_b(PROBE_CLIP);
    (
        units[0].to_vec(),
        units[1].to_vec(),
        units[2..].iter().map(|u| u.to_vec()).collect(),
    )
}

/// A description from the probe clip's parameter sets, with 4-byte lengths.
fn description() -> CFTypeRef {
    corefoundation::bootstrap();
    let (sps, pps, _) = probe_units();
    let pointers = [sps.as_ptr(), pps.as_ptr()];
    let sizes = [sps.len(), pps.len()];
    let mut out = std::ptr::null();
    // SAFETY: two arrays of two entries and an out-parameter.
    let status = unsafe {
        coremedia::CMVideoFormatDescriptionCreateFromH264ParameterSets(
            std::ptr::null(),
            2,
            pointers.as_ptr(),
            sizes.as_ptr(),
            4,
            &mut out,
        )
    };
    assert_eq!(status, 0);
    out
}

fn avcc(units: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for unit in units {
        out.extend_from_slice(&(unit.len() as u32).to_be_bytes());
        out.extend_from_slice(unit);
    }
    out
}

/// A sample the way Roblox makes one: its bytes borrowed into a block
/// buffer with `kCFAllocatorNull`, copied into a contiguous one, and that
/// wrapped with one timing entry and one size.
fn sample(format: CFTypeRef, payload: &[u8], display: u64) -> CFTypeRef {
    let mut memory = payload.to_vec();
    let (mut block, mut contiguous, mut sample) =
        (std::ptr::null(), std::ptr::null(), std::ptr::null());
    let timing = TimingInfo {
        duration: CMTimeMake(FRAME_NS, NS),
        presentation_time_stamp: CMTimeMake(display as i64 * FRAME_NS, NS),
        decode_time_stamp: coremedia::time::INVALID,
    };
    let size = memory.len();
    // SAFETY: live memory and out-parameters; the borrowed block is copied
    // before the memory goes.
    unsafe {
        assert_eq!(
            coremedia::block::CMBlockBufferCreateWithMemoryBlock(
                std::ptr::null(),
                memory.as_mut_ptr().cast(),
                size,
                std::ptr::null(),
                std::ptr::null(),
                0,
                size,
                0,
                &mut block,
            ),
            0
        );
        assert_eq!(
            coremedia::block::CMBlockBufferCreateContiguous(
                std::ptr::null(),
                block,
                std::ptr::null(),
                std::ptr::null(),
                0,
                0,
                coremedia::block::ALWAYS_COPY_DATA,
                &mut contiguous,
            ),
            0
        );
        CFRelease(block);
        drop(memory);
        assert_eq!(
            coremedia::sample::CMSampleBufferCreate(
                std::ptr::null(),
                contiguous,
                1,
                std::ptr::null(),
                std::ptr::null_mut(),
                format,
                1,
                1,
                &timing,
                1,
                &size,
                &mut sample,
            ),
            0
        );
        CFRelease(contiguous);
    }
    sample
}

/// A sample for the mock decoder: one slice whose second byte is the
/// display index.
fn mock_sample(format: CFTypeRef, display: u64) -> CFTypeRef {
    let header = if display == 0 { 0x65 } else { 0x41 };
    sample(format, &avcc(&[&[header, display as u8]]), display)
}

struct Open {
    session: CFTypeRef,
    format: CFTypeRef,
    recorder: Box<Recorder>,
}

impl Drop for Open {
    fn drop(&mut self) {
        release(self.session);
        release(self.format);
    }
}

fn create_on(
    registry: &Registry,
    specification: CFTypeRef,
    attributes: CFTypeRef,
) -> Result<Open, i32> {
    let format = description();
    let recorder = Box::new(Recorder::default());
    let record = CallbackRecord {
        callback: Some(record),
        refcon: &*recorder as *const Recorder as *mut c_void,
    };
    let mut session = std::ptr::null();
    // SAFETY: a description, dictionaries or null, a record and an
    // out-parameter.
    let status = unsafe {
        create_with(
            registry,
            format,
            specification,
            attributes,
            &record,
            &mut session,
        )
    };
    if status != 0 {
        assert!(session.is_null());
        release(format);
        return Err(status);
    }
    recorder.session.store(session as usize, Ordering::Relaxed);
    Ok(Open {
        session,
        format,
        recorder,
    })
}

fn open(backend: MockBackend, attributes: CFTypeRef) -> Open {
    let registry = Registry::new(vec![backend.leak()]);
    create_on(&registry, std::ptr::null(), attributes).expect("a session")
}

impl Open {
    fn decode(&self, sample: CFTypeRef, flags: u32) -> i32 {
        let mut info = u32::MAX;
        // SAFETY: the session, a sample and an out-parameter.
        let status = unsafe {
            VTDecompressionSessionDecodeFrame(
                self.session,
                sample,
                flags,
                sample as *mut c_void,
                &mut info,
            )
        };
        assert_eq!(info, 0, "decoding is synchronous");
        status
    }

    fn decode_shown(&self, displays: &[u64]) -> Vec<CFTypeRef> {
        displays
            .iter()
            .map(|&display| {
                let sample = mock_sample(self.format, display);
                assert_eq!(self.decode(sample, DECODE_ASYNCHRONOUS), 0);
                sample
            })
            .collect()
    }

    fn finish(&self) -> i32 {
        // SAFETY: the session.
        unsafe { VTDecompressionSessionFinishDelayedFrames(self.session) }
    }

    fn property(&self, name: &str) -> Result<CFTypeRef, i32> {
        let mut out = std::ptr::null();
        // SAFETY: the session, a key and an out-parameter.
        match unsafe { VTSessionCopyProperty(self.session, key(name), std::ptr::null(), &mut out) }
        {
            0 => Ok(out),
            status => Err(status),
        }
    }
}

fn release_all(samples: Vec<CFTypeRef>) {
    samples.into_iter().for_each(release);
}

/// The Y, Cb and Cr samples of a buffer, read the way a guest reads them.
fn yuv(image: CFTypeRef) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    // SAFETY: a pixel buffer the session made, locked while it is read.
    unsafe {
        assert_eq!(
            pb::CVPixelBufferLockBaseAddress(image, pb::LOCK_READ_ONLY),
            0
        );
        let plane = |index: usize| {
            let (w, h) = (
                pb::CVPixelBufferGetWidthOfPlane(image, index),
                pb::CVPixelBufferGetHeightOfPlane(image, index),
            );
            let base = pb::CVPixelBufferGetBaseAddressOfPlane(image, index) as *const u8;
            let row = pb::CVPixelBufferGetBytesPerRowOfPlane(image, index);
            let width = w * if pb::CVPixelBufferGetPlaneCount(image) == 2 && index == 1 {
                2
            } else {
                1
            };
            let mut out = Vec::with_capacity(width * h);
            for y in 0..h {
                out.extend_from_slice(std::slice::from_raw_parts(base.add(y * row), width));
            }
            out
        };
        let y = plane(0);
        let (cb, cr) = match pb::CVPixelBufferGetPlaneCount(image) {
            2 => {
                let uv = plane(1);
                (
                    uv.iter().step_by(2).copied().collect(),
                    uv.iter().skip(1).step_by(2).copied().collect(),
                )
            }
            _ => (plane(1), plane(2)),
        };
        assert_eq!(
            pb::CVPixelBufferUnlockBaseAddress(image, pb::LOCK_READ_ONLY),
            0
        );
        (y, cb, cr)
    }
}

fn format_of(image: CFTypeRef) -> u32 {
    // SAFETY: a pixel buffer.
    unsafe { pb::CVPixelBufferGetPixelFormatType(image) }
}

#[test]
fn no_name_is_exported_twice() {
    let mut names = crate::names();
    let count = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), count);
    let symbols = crate::data_symbols();
    let mut data: Vec<&str> = symbols.iter().map(|(n, _)| n.as_str()).collect();
    data.sort_unstable();
    data.dedup();
    assert_eq!(data.len(), symbols.len());
    assert!(crate::symbol("VTDecompressionSessionDecodeFrame").is_some());
}

#[test]
fn each_successful_decode_gets_exactly_one_callback() {
    let s = open(
        MockBackend {
            delay: 2,
            ..MockBackend::new("mock")
        },
        std::ptr::null(),
    );
    let samples = s.decode_shown(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    assert_eq!(s.recorder.frames.lock().unwrap().len(), 8, "two are held");
    assert_eq!(s.finish(), 0);
    let frames = s.recorder.take();
    assert_eq!(frames.len(), samples.len());
    let mut sources: Vec<usize> = frames.iter().map(|f| f.source).collect();
    sources.sort_unstable();
    let mut want: Vec<usize> = samples.iter().map(|&s| s as usize).collect();
    want.sort_unstable();
    assert_eq!(sources, want);
    assert!(frames.iter().all(|f| f.status == 0 && f.image != 0));
    assert!(
        frames
            .iter()
            .all(|f| f.flags == INFO_IMAGE_BUFFER_MODIFIABLE)
    );
    release_all(samples);
}

#[test]
fn reordered_frames_come_out_in_presentation_order_with_their_refcons() {
    let s = open(
        MockBackend {
            delay: 2,
            ..MockBackend::new("mock")
        },
        std::ptr::null(),
    );
    let order = [0, 3, 1, 2, 5, 4];
    let samples = s.decode_shown(&order);
    assert_eq!(s.finish(), 0);
    let frames = s.recorder.take();
    assert_eq!(frames.len(), 6);
    for (shown, frame) in frames.iter().enumerate() {
        let coded = order.iter().position(|&d| d == shown as u64).unwrap();
        assert_eq!(frame.source, samples[coded] as usize, "frame {shown}");
        assert_eq!(frame.pts, CMTimeMake(shown as i64 * FRAME_NS, NS));
        assert_eq!(frame.duration, CMTimeMake(FRAME_NS, NS));
        let (y, _, _) = yuv(frame.image as CFTypeRef);
        assert!(y.iter().all(|&v| v == mock::luma(shown as u64)));
    }
    release_all(samples);
}

#[test]
fn do_not_output_frame_is_reported_dropped_without_image() {
    let s = open(MockBackend::new("mock"), std::ptr::null());
    let shown = mock_sample(s.format, 0);
    let hidden = mock_sample(s.format, 1);
    assert_eq!(s.decode(shown, 0), 0);
    assert_eq!(s.decode(hidden, DECODE_DO_NOT_OUTPUT_FRAME), 0);
    let frames = s.recorder.take();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        (frames[0].status, frames[0].flags),
        (0, INFO_IMAGE_BUFFER_MODIFIABLE)
    );
    assert_eq!(frames[1].source, hidden as usize);
    assert_eq!(
        (frames[1].status, frames[1].flags, frames[1].image),
        (0, INFO_FRAME_DROPPED, 0)
    );
    release_all(vec![shown, hidden]);
}

#[test]
fn finish_delayed_frames_emits_held_frames_and_fails_the_rest() {
    let s = open(
        MockBackend {
            delay: 3,
            lose_at: Some(1),
            ..MockBackend::new("mock")
        },
        std::ptr::null(),
    );
    let samples = s.decode_shown(&[0, 1, 2]);
    assert!(s.recorder.take().is_empty());
    assert_eq!(s.finish(), 0);
    let frames = s.recorder.take();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0].source, samples[0] as usize);
    assert_eq!(frames[1].source, samples[2] as usize);
    assert!(frames[..2].iter().all(|f| f.status == 0 && f.image != 0));
    assert_eq!(frames[2].source, samples[1] as usize);
    assert_eq!(
        (frames[2].status, frames[2].flags, frames[2].image),
        (BAD_DATA, INFO_FRAME_DROPPED, 0)
    );
    assert_eq!(s.finish(), 0, "nothing is left to finish");
    assert!(s.recorder.take().is_empty());
    let after = s.decode_shown(&[3]);
    assert_eq!(
        s.recorder.take().len(),
        0,
        "held again until the next finish"
    );
    release_all(samples);
    release_all(after);
}

#[test]
fn invalidate_stops_callbacks_and_later_calls_return_invalid_session() {
    let s = open(
        MockBackend {
            delay: 3,
            ..MockBackend::new("mock")
        },
        std::ptr::null(),
    );
    let samples = s.decode_shown(&[0, 1]);
    // SAFETY: the session.
    unsafe {
        VTDecompressionSessionInvalidate(s.session);
        VTDecompressionSessionInvalidate(s.session);
        let late = mock_sample(s.format, 2);
        assert_eq!(s.decode(late, 0), INVALID_SESSION);
        release(late);
        assert_eq!(s.finish(), INVALID_SESSION);
        assert_eq!(
            VTDecompressionSessionWaitForAsynchronousFrames(s.session),
            INVALID_SESSION
        );
        assert_eq!(
            VTDecompressionSessionCanAcceptFormatDescription(s.session, s.format),
            0
        );
    }
    assert_eq!(s.property(USING_HARDWARE), Err(INVALID_SESSION));
    assert!(
        s.recorder.take().is_empty(),
        "held frames go without a callback"
    );
    release_all(samples);
}

#[test]
fn invalidate_from_inside_the_callback_does_not_deadlock() {
    let s = open(
        MockBackend {
            delay: 3,
            ..MockBackend::new("mock")
        },
        std::ptr::null(),
    );
    let samples = s.decode_shown(&[0, 1, 2]);
    s.recorder
        .invalidate_in_callback
        .store(true, Ordering::Relaxed);
    assert_eq!(s.finish(), 0);
    let frames = s.recorder.take();
    assert_eq!(
        frames.len(),
        1,
        "no callback after the one that invalidated"
    );
    assert_eq!(s.finish(), INVALID_SESSION);
    release_all(samples);
}

#[test]
fn decoder_bad_data_goes_through_the_callback() {
    let s = open(
        MockBackend {
            bad_data_at: Some(1),
            ..MockBackend::new("mock")
        },
        std::ptr::null(),
    );
    let samples = s.decode_shown(&[0, 1, 2]);
    let frames = s.recorder.take();
    let got: Vec<(usize, i32, u32, bool)> = frames
        .iter()
        .map(|f| (f.source, f.status, f.flags, f.image != 0))
        .collect();
    assert_eq!(
        got,
        [
            (samples[0] as usize, 0, INFO_IMAGE_BUFFER_MODIFIABLE, true),
            (samples[1] as usize, BAD_DATA, INFO_FRAME_DROPPED, false),
            (samples[2] as usize, 0, INFO_IMAGE_BUFFER_MODIFIABLE, true),
        ]
    );
    // A sample that is not one, and one whose lengths are wrong, are refused
    // without a callback.
    let truncated = sample(s.format, &[0, 0, 0, 9, 0x41], 3);
    assert_eq!(s.decode(truncated, 0), BAD_DATA);
    assert_eq!(s.decode(s.format, 0), BAD_DATA);
    assert!(s.recorder.take().is_empty());
    release(truncated);
    release_all(samples);
}

#[test]
fn malfunction_fails_pending_and_poisons_the_session() {
    let backend = MockBackend {
        delay: 2,
        malfunction_at: Some((2, true)),
        ..MockBackend::new("mock")
    };
    let unhealthy = backend.unhealthy;
    let s = open(backend, std::ptr::null());
    let samples = s.decode_shown(&[0, 1]);
    let broken = mock_sample(s.format, 2);
    assert_eq!(s.decode(broken, 0), MALFUNCTION);
    let frames = s.recorder.take();
    let got: Vec<(usize, i32, bool)> = frames
        .iter()
        .map(|f| (f.source, f.status, f.image != 0))
        .collect();
    assert_eq!(
        got,
        [
            (samples[0] as usize, MALFUNCTION, false),
            (samples[1] as usize, MALFUNCTION, false),
        ]
    );
    let later = mock_sample(s.format, 3);
    assert_eq!(s.decode(later, 0), MALFUNCTION);
    assert_eq!(s.finish(), 0);
    assert!(s.recorder.take().is_empty());
    assert!(
        unhealthy.load(Ordering::Relaxed),
        "a global failure is the backend's"
    );
    release_all(samples);
    release_all(vec![broken, later]);
}

#[test]
fn format_change_is_refused_and_can_accept_is_conservative() {
    let s = open(MockBackend::new("mock"), std::ptr::null());
    let mut other = std::ptr::null();
    // SAFETY: an out-parameter.
    unsafe {
        coremedia::CMVideoFormatDescriptionCreate(
            std::ptr::null(),
            coremedia::CODEC_H264,
            32,
            32,
            std::ptr::null(),
            &mut other,
        );
    }
    let same = description();
    let foreign = mock_sample(other, 0);
    assert_eq!(
        s.decode(foreign, 0),
        FORMAT_DESCRIPTION_CHANGE_NOT_SUPPORTED
    );
    let equal = mock_sample(same, 0);
    assert_eq!(
        s.decode(equal, 0),
        0,
        "an equal description is the same format"
    );
    // SAFETY: the session and descriptions.
    unsafe {
        assert_eq!(
            VTDecompressionSessionCanAcceptFormatDescription(s.session, s.format),
            1
        );
        assert_eq!(
            VTDecompressionSessionCanAcceptFormatDescription(s.session, same),
            1
        );
        assert_eq!(
            VTDecompressionSessionCanAcceptFormatDescription(s.session, other),
            0
        );
    }
    release_all(vec![foreign, equal, other, same]);
}

#[test]
fn y420_output_deinterleaves_chroma() {
    let s = open(MockBackend::new("mock"), attributes(Y420, false));
    let samples = s.decode_shown(&[1]);
    let frames = s.recorder.take();
    let image = frames[0].image as CFTypeRef;
    assert_eq!(format_of(image), Y420);
    // SAFETY: a pixel buffer.
    assert!(unsafe { pb::CVPixelBufferGetIOSurface(image) }.is_null());
    let (y, cb, cr) = yuv(image);
    assert_eq!((y.len(), cb.len(), cr.len()), (64 * 64, 32 * 32, 32 * 32));
    assert!(y.iter().all(|&v| v == mock::luma(1)));
    assert!(cb.iter().all(|&v| v == 90));
    assert!(cr.iter().all(|&v| v == 170));
    release_all(samples);
}

#[test]
fn i420_pictures_interleave_into_420v() {
    let s = open(
        MockBackend {
            i420: true,
            ..MockBackend::new("cpu-like").software()
        },
        attributes(NV12_VIDEO, true),
    );
    let samples = s.decode_shown(&[0]);
    let frames = s.recorder.take();
    let image = frames[0].image as CFTypeRef;
    assert_eq!(format_of(image), NV12_VIDEO);
    // SAFETY: a pixel buffer.
    assert!(!unsafe { pb::CVPixelBufferGetIOSurface(image) }.is_null());
    let (_, cb, cr) = yuv(image);
    assert!(cb.iter().all(|&v| v == 90) && cr.iter().all(|&v| v == 170));
    release_all(samples);
}

#[test]
fn full_range_stream_into_y420_is_range_mapped() {
    let s = open(
        MockBackend {
            colour: Colour {
                full_range: Some(true),
                ..Colour::UNSPECIFIED
            },
            luma: Some(255),
            chroma: (0, 255),
            ..MockBackend::new("mock")
        },
        attributes(Y420, true),
    );
    let samples = s.decode_shown(&[0]);
    let frames = s.recorder.take();
    let (y, cb, cr) = yuv(frames[0].image as CFTypeRef);
    assert!(y.iter().all(|&v| v == 235));
    assert!(cb.iter().all(|&v| v == 16));
    assert!(cr.iter().all(|&v| v == 240));
    release_all(samples);
}

#[test]
fn unrequested_format_follows_the_stream_range() {
    for (full_range, want) in [
        (None, NV12_VIDEO),
        (Some(false), NV12_VIDEO),
        (Some(true), NV12_FULL),
    ] {
        let s = open(
            MockBackend {
                colour: Colour {
                    full_range,
                    ..Colour::UNSPECIFIED
                },
                ..MockBackend::new("mock")
            },
            std::ptr::null(),
        );
        let samples = s.decode_shown(&[0, 1]);
        let frames = s.recorder.take();
        assert!(
            frames
                .iter()
                .all(|f| format_of(f.image as CFTypeRef) == want)
        );
        let (y, _, _) = yuv(frames[1].image as CFTypeRef);
        assert!(y.iter().all(|&v| v == mock::luma(1)), "not range mapped");
        release_all(samples);
    }
}

#[test]
fn bgra_output_of_bt709_limited_gray_is_gray() {
    let s = open(
        MockBackend {
            colour: Colour {
                full_range: Some(false),
                matrix: 1,
                primaries: 1,
                transfer: 1,
            },
            luma: Some(126),
            chroma: (128, 128),
            ..MockBackend::new("mock")
        },
        attributes(BGRA, true),
    );
    let samples = s.decode_shown(&[0]);
    let frames = s.recorder.take();
    let image = frames[0].image as CFTypeRef;
    assert_eq!(format_of(image), BGRA);
    // SAFETY: a packed pixel buffer, read while locked.
    unsafe {
        assert_eq!(
            pb::CVPixelBufferLockBaseAddress(image, pb::LOCK_READ_ONLY),
            0
        );
        let base = pb::CVPixelBufferGetBaseAddress(image) as *const u8;
        let row = pb::CVPixelBufferGetBytesPerRow(image);
        for y in 0..64 {
            let line = std::slice::from_raw_parts(base.add(y * row), 64 * 4);
            assert!(
                line.chunks(4).all(|p| p == [128, 128, 128, 255]),
                "row {y}: {:?}",
                &line[..8]
            );
        }
        assert_eq!(
            pb::CVPixelBufferUnlockBaseAddress(image, pb::LOCK_READ_ONLY),
            0
        );
    }
    let attachments =
        corevideo::buffer::copy_attachments(image, corevideo::buffer::SHOULD_PROPAGATE);
    assert!(!attachments.is_null());
    // SAFETY: the dictionary just copied.
    unsafe {
        let matrix =
            corefoundation::api::CFDictionaryGetValue(attachments, key("CVImageBufferYCbCrMatrix"));
        assert_ne!(corefoundation::api::CFEqual(matrix, key("ITU_R_709_2")), 0);
        CFRelease(attachments);
    }
    release_all(samples);
}

#[test]
fn description_without_avcc_finds_no_decoder() {
    corefoundation::bootstrap();
    let registry = Registry::new(vec![MockBackend::new("mock").leak()]);
    let mut format = std::ptr::null();
    let mut session: CFTypeRef = 1 as CFTypeRef;
    let recorder = Recorder::default();
    let record = CallbackRecord {
        callback: Some(record),
        refcon: &recorder as *const Recorder as *mut c_void,
    };
    // SAFETY: out-parameters, a description and a record.
    unsafe {
        coremedia::CMVideoFormatDescriptionCreate(
            std::ptr::null(),
            coremedia::CODEC_H264,
            64,
            64,
            std::ptr::null(),
            &mut format,
        );
        let status = create_with(
            &registry,
            format,
            std::ptr::null(),
            std::ptr::null(),
            &record,
            &mut session,
        );
        assert_eq!(status, COULD_NOT_FIND_VIDEO_DECODER);
        assert!(session.is_null());
        CFRelease(format);
    }
}

#[test]
fn no_backend_or_an_unsupported_output_finds_no_decoder() {
    assert!(matches!(
        create_on(
            &Registry::from_names("none"),
            std::ptr::null(),
            std::ptr::null()
        ),
        Err(COULD_NOT_FIND_VIDEO_DECODER)
    ));
    let registry = Registry::new(vec![MockBackend::new("mock").leak()]);
    let rgb = attributes(u32::from_be_bytes(*b"RGBA"), false);
    assert!(matches!(
        create_on(&registry, std::ptr::null(), rgb),
        Err(COULD_NOT_FIND_VIDEO_DECODER)
    ));
    let software_only =
        key_number(&[(ENABLE_HARDWARE, corefoundation::collections::boolean(false))]);
    assert!(matches!(
        create_on(&registry, software_only, std::ptr::null()),
        Err(COULD_NOT_FIND_VIDEO_DECODER)
    ));
    let require = key_number(&[(REQUIRE_HARDWARE, corefoundation::collections::boolean(true))]);
    assert!(create_on(&registry, require, std::ptr::null()).is_ok());
}

#[test]
fn copy_property_reports_hardware_and_realtime() {
    let hardware = open(MockBackend::new("hard"), std::ptr::null());
    let software = open(MockBackend::new("soft").software(), std::ptr::null());
    let yes = corefoundation::collections::boolean(true);
    let no = corefoundation::collections::boolean(false);
    assert_eq!(hardware.property(USING_HARDWARE), Ok(yes));
    assert_eq!(software.property(USING_HARDWARE), Ok(no));
    assert_eq!(hardware.property(REAL_TIME), Ok(no));
    // SAFETY: the session, keys and values.
    unsafe {
        assert_eq!(
            VTSessionSetProperty(hardware.session, key(REAL_TIME), yes),
            0
        );
        assert_eq!(
            VTSessionSetProperty(
                hardware.session,
                key("OutputPoolRequestedMinimumBufferCount"),
                yes
            ),
            PROPERTY_NOT_SUPPORTED
        );
        let mut out = std::ptr::null();
        assert_eq!(
            VTSessionCopyProperty(hardware.format, key(REAL_TIME), std::ptr::null(), &mut out),
            PARAMETER_ERR,
            "a description is not a session"
        );
    }
    assert_eq!(hardware.property(REAL_TIME), Ok(yes));
    assert_eq!(
        hardware.property("NumberOfFramesBeingDecoded"),
        Err(PROPERTY_NOT_SUPPORTED)
    );
    // SAFETY: nothing.
    assert_ne!(unsafe { VTDecompressionSessionGetTypeID() }, 0);
}

/// The probe clip through a session on `registry` with `attributes`, as
/// Roblox drives one: decode each sample with flags 9 (asynchronous and
/// temporal processing), then finish, wait and invalidate. Checks every picture against what the clip was made with.
fn decode_probe(registry: &Registry, attributes: CFTypeRef, want_format: u32, iosurface: bool) {
    let s = create_on(registry, std::ptr::null(), attributes).expect("a session");
    assert_eq!(
        s.property(USING_HARDWARE),
        Ok(corefoundation::collections::boolean(true))
    );
    let (_, _, slices) = probe_units();
    let samples: Vec<CFTypeRef> = slices
        .iter()
        .zip(PROBE_DISPLAY_ORDER)
        .map(|(slice, display)| sample(s.format, &avcc(&[slice]), display))
        .collect();
    for &sample in &samples {
        assert_eq!(
            s.decode(sample, DECODE_ASYNCHRONOUS | DECODE_TEMPORAL_PROCESSING),
            0
        );
    }
    assert_eq!(s.finish(), 0);
    // SAFETY: the session.
    unsafe {
        assert_eq!(
            VTDecompressionSessionWaitForAsynchronousFrames(s.session),
            0
        );
        VTDecompressionSessionInvalidate(s.session);
    }
    let frames = s.recorder.take();
    assert_eq!(frames.len(), 6);
    for (shown, frame) in frames.iter().enumerate() {
        let coded = PROBE_DISPLAY_ORDER
            .iter()
            .position(|&d| d == shown as u64)
            .unwrap();
        assert_eq!(frame.status, 0, "frame {shown}");
        assert_eq!(
            frame.pts,
            CMTimeMake(shown as i64 * FRAME_NS, NS),
            "frame {shown}"
        );
        assert_eq!(frame.source, samples[coded] as usize, "frame {shown}");
        let image = frame.image as CFTypeRef;
        assert_eq!(format_of(image), want_format);
        // SAFETY: a pixel buffer.
        unsafe {
            assert_eq!(
                (
                    pb::CVPixelBufferGetWidth(image),
                    pb::CVPixelBufferGetHeight(image)
                ),
                (64, 64)
            );
            assert_eq!(!pb::CVPixelBufferGetIOSurface(image).is_null(), iosurface);
        }
        let (y, cb, cr) = yuv(image);
        let luma = 40 + 30 * shown as u8;
        let near = |v: u8, want: u8| v.abs_diff(want) <= 2;
        assert!(
            y.iter().all(|&v| near(v, luma)),
            "frame {shown}: luma {:?}, not {luma}",
            &y[..8]
        );
        for row in 0..32 {
            for column in 0..32 {
                let at = row * 32 + column;
                let (want_cb, want_cr) = (
                    if column < 16 { 90 } else { 170 },
                    if row < 16 { 100 } else { 160 },
                );
                assert!(
                    near(cb[at], want_cb) && near(cr[at], want_cr),
                    "frame {shown} at chroma ({column}, {row}): ({}, {})",
                    cb[at],
                    cr[at]
                );
            }
        }
    }
    drop(frames);
    release_all(samples);
}

/// The probe clip on one hardware backend, in each output the session
/// makes: Roblox's `y420` on an IOSurface, `420v`, and no attributes at all.
fn hardware_decodes_the_probe_clip_for(name: &str) {
    let registry = Registry::from_names(name);
    let backend = registry.backends()[0];
    if let Err(why) = backend.available() {
        eprintln!("skipped: {name} cannot decode H.264 here: {why}");
        return;
    }
    eprintln!("{name}: decoding through {}", backend.description());
    decode_probe(&registry, attributes(Y420, true), Y420, true);
    decode_probe(&registry, attributes(NV12_VIDEO, false), NV12_VIDEO, false);
    decode_probe(&registry, std::ptr::null(), NV12_VIDEO, false);
}

#[test]
fn hardware_decodes_the_probe_clip_for_vulkan() {
    hardware_decodes_the_probe_clip_for("vulkan");
}

#[test]
fn hardware_decodes_the_probe_clip_for_vaapi() {
    hardware_decodes_the_probe_clip_for("vaapi");
}

#[test]
fn hardware_decodes_the_probe_clip_for_cuda() {
    hardware_decodes_the_probe_clip_for("cuda");
}
