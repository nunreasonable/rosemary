//! `CMSampleBuffer`: samples of one format, with their timing and sizes, over
//! a block buffer.
//!
//! Only sample buffers whose data is ready when they are made: a
//! make-data-ready callback is refused, as is everything that would make an
//! image sample buffer. A sample buffer here is never invalidated, so every
//! live one is valid.

use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use corefoundation::base::{CFTypeID, CFTypeRef};
use corefoundation::callbacks::Policy;
use objc::class::Class;
use objc::object::Id;

use crate::Time;

/// `OSStatus` values from `CMSampleBuffer.h`.
pub const ALLOCATION_FAILED: i32 = -12730;
pub const REQUIRED_PARAMETER_MISSING: i32 = -12731;
pub const INVALID_ENTRY_COUNT: i32 = -12738;
const NO_ERR: i32 = 0;

/// The per-sample attachment keys, as `(Mach-O name, text)`; the texts are
/// Apple's.
pub(crate) const KEYS: &[(&str, &str)] = &[
    ("_kCMSampleAttachmentKey_NotSync", "NotSync"),
    ("_kCMSampleAttachmentKey_DependsOnOthers", "DependsOnOthers"),
    (
        "_kCMSampleAttachmentKey_IsDependedOnByOthers",
        "IsDependedOnByOthers",
    ),
    ("_kCMSampleAttachmentKey_DoNotDisplay", "DoNotDisplay"),
    (
        "_kCMSampleAttachmentKey_DisplayImmediately",
        "DisplayImmediately",
    ),
];

/// `CMSampleTimingInfo`: seventy-two bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimingInfo {
    pub duration: Time,
    pub presentation_time_stamp: Time,
    pub decode_time_stamp: Time,
}

#[repr(C)]
struct Sample {
    isa: *const c_void,
    state: State,
}

struct State {
    /// A retained block buffer, or null.
    data: CFTypeRef,
    /// A retained format description, or null.
    format: CFTypeRef,
    ready: bool,
    num_samples: i64,
    timing: Vec<TimingInfo>,
    /// Nothing reads the sizes yet; they are kept for the getters that will.
    #[allow(dead_code)]
    sizes: Vec<usize>,
    /// A retained array of `num_samples` mutable dictionaries, made on first
    /// request.
    attachments: Mutex<usize>,
}

impl Drop for State {
    fn drop(&mut self) {
        crate::release(self.data);
        crate::release(self.format);
        crate::release(
            *self
                .attachments
                .get_mut()
                .unwrap_or_else(|e| e.into_inner()) as CFTypeRef,
        );
    }
}

fn class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let p = *CLASS.get_or_init(|| {
        corefoundation::bootstrap();
        let class =
            corefoundation::base::register(c"CMSampleBuffer", None, size_of::<Sample>(), true);
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
    // SAFETY: `-dealloc` of a `CMSampleBuffer`, whose body is `Sample`.
    let sample = unsafe { &mut *(receiver as *mut Sample) };
    // SAFETY: written once when the buffer was made, dropped only here.
    unsafe { std::ptr::drop_in_place(&mut sample.state) };
    objc::arc::finalize(receiver);
}

fn state<'a>(buffer: CFTypeRef) -> Option<&'a State> {
    // SAFETY: null or an object a guest received from this runtime.
    let is_sample =
        !buffer.is_null() && unsafe { corefoundation::base::is_kind_of(buffer, class()) };
    // SAFETY: checked to be a `CMSampleBuffer`, whose body is `Sample`.
    is_sample.then(|| unsafe { &(*(buffer as *const Sample)).state })
}

/// Whether `object` is a sample buffer.
pub fn is_sample_buffer(object: CFTypeRef) -> bool {
    state(object).is_some()
}

/// A sample buffer's block buffer, not retained; null for none.
pub fn data(buffer: CFTypeRef) -> CFTypeRef {
    state(buffer).map_or(std::ptr::null(), |s| s.data)
}

/// A sample buffer's format description, not retained; null for none.
pub fn format(buffer: CFTypeRef) -> CFTypeRef {
    state(buffer).map_or(std::ptr::null(), |s| s.format)
}

/// The first sample's presentation time stamp and duration, each
/// `kCMTimeInvalid` where the buffer has no timing.
pub fn timing(buffer: CFTypeRef) -> (Time, Time) {
    state(buffer)
        .and_then(|s| s.timing.first())
        .map_or((crate::time::INVALID, crate::time::INVALID), |t| {
            (t.presentation_time_stamp, t.duration)
        })
}

/// `OSStatus CMSampleBufferCreate(CFAllocatorRef, CMBlockBufferRef dataBuffer,
/// Boolean dataReady, CMSampleBufferMakeDataReadyCallback, void *refcon,
/// CMFormatDescriptionRef, CMItemCount numSamples, CMItemCount
/// numSampleTimingEntries, const CMSampleTimingInfo *, CMItemCount
/// numSampleSizeEntries, const size_t *, CMSampleBufferRef *)`.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn CMSampleBufferCreate(
    _allocator: CFTypeRef,
    data_buffer: CFTypeRef,
    data_ready: u8,
    make_data_ready: *const c_void,
    _make_data_ready_refcon: *mut c_void,
    format: CFTypeRef,
    num_samples: i64,
    num_timing_entries: i64,
    timing_array: *const TimingInfo,
    num_size_entries: i64,
    size_array: *const usize,
    out: *mut CFTypeRef,
) -> i32 {
    if out.is_null() {
        return REQUIRED_PARAMETER_MISSING;
    }
    if !make_data_ready.is_null() {
        eprintln!(
            "rosemary: CMSampleBufferCreate: a make-data-ready callback is not supported; \
             no sample buffer is made"
        );
        return REQUIRED_PARAMETER_MISSING;
    }
    let count_ok = |count: i64| count == 0 || count == 1 || count == num_samples;
    if num_samples < 0 || !count_ok(num_timing_entries) || !count_ok(num_size_entries) {
        return INVALID_ENTRY_COUNT;
    }
    if (num_timing_entries > 0 && timing_array.is_null())
        || (num_size_entries > 0 && size_array.is_null())
    {
        return REQUIRED_PARAMETER_MISSING;
    }
    // SAFETY: the caller's arrays of the counts it gave, checked non-null
    // where non-empty.
    let (timing, sizes) = unsafe {
        let timing = match num_timing_entries {
            0 => Vec::new(),
            n => std::slice::from_raw_parts(timing_array, n as usize).to_vec(),
        };
        let sizes = match num_size_entries {
            0 => Vec::new(),
            n => std::slice::from_raw_parts(size_array, n as usize).to_vec(),
        };
        (timing, sizes)
    };
    let retain = |object: CFTypeRef| match object.is_null() {
        true => object,
        // SAFETY: a CF object the caller passed.
        false => unsafe { corefoundation::api::CFRetain(object) },
    };
    // SAFETY: `Sample` is the registered layout.
    let sample = unsafe { corefoundation::base::allocate::<Sample>(class()) };
    // SAFETY: a fresh, zeroed allocation whose state is written once here.
    unsafe {
        std::ptr::write(
            &mut (*sample).state,
            State {
                data: retain(data_buffer),
                format: retain(format),
                ready: data_ready != 0,
                num_samples,
                timing,
                sizes,
                attachments: Mutex::new(0),
            },
        )
    };
    // SAFETY: the caller's out-parameter, checked above.
    unsafe { out.write(sample as CFTypeRef) };
    NO_ERR
}

/// `CFTypeID CMSampleBufferGetTypeID(void)`.
pub unsafe extern "C" fn CMSampleBufferGetTypeID() -> CFTypeID {
    corefoundation::base::type_id(class())
}

/// `Boolean CMSampleBufferIsValid(CMSampleBufferRef)`.
pub unsafe extern "C" fn CMSampleBufferIsValid(buffer: CFTypeRef) -> u8 {
    u8::from(state(buffer).is_some())
}

/// `Boolean CMSampleBufferDataIsReady(CMSampleBufferRef)`.
pub unsafe extern "C" fn CMSampleBufferDataIsReady(buffer: CFTypeRef) -> u8 {
    u8::from(state(buffer).is_some_and(|s| s.ready))
}

/// `CMItemCount CMSampleBufferGetNumSamples(CMSampleBufferRef)`.
pub unsafe extern "C" fn CMSampleBufferGetNumSamples(buffer: CFTypeRef) -> i64 {
    state(buffer).map_or(0, |s| s.num_samples)
}

/// `CMBlockBufferRef CMSampleBufferGetDataBuffer(CMSampleBufferRef)`: not
/// retained.
pub unsafe extern "C" fn CMSampleBufferGetDataBuffer(buffer: CFTypeRef) -> CFTypeRef {
    data(buffer)
}

/// `CMFormatDescriptionRef CMSampleBufferGetFormatDescription(CMSampleBufferRef)`:
/// not retained.
pub unsafe extern "C" fn CMSampleBufferGetFormatDescription(buffer: CFTypeRef) -> CFTypeRef {
    format(buffer)
}

/// `CVImageBufferRef CMSampleBufferGetImageBuffer(CMSampleBufferRef)`: null,
/// since no image sample buffer is ever made.
pub unsafe extern "C" fn CMSampleBufferGetImageBuffer(_buffer: CFTypeRef) -> CFTypeRef {
    std::ptr::null()
}

/// `CMTime CMSampleBufferGetPresentationTimeStamp(CMSampleBufferRef)`: the
/// first timing entry's, or `kCMTimeInvalid` without one.
pub unsafe extern "C" fn CMSampleBufferGetPresentationTimeStamp(buffer: CFTypeRef) -> Time {
    timing(buffer).0
}

/// `CFArrayRef CMSampleBufferGetSampleAttachmentsArray(CMSampleBufferRef,
/// Boolean createIfNecessary)`: an array of one mutable dictionary per
/// sample, owned by the sample buffer, made only when asked to.
pub unsafe extern "C" fn CMSampleBufferGetSampleAttachmentsArray(
    buffer: CFTypeRef,
    create_if_necessary: u8,
) -> CFTypeRef {
    let Some(state) = state(buffer) else {
        return std::ptr::null();
    };
    let mut attachments = state.attachments.lock().unwrap_or_else(|e| e.into_inner());
    if *attachments == 0 && create_if_necessary != 0 {
        let dictionaries: Vec<CFTypeRef> = (0..state.num_samples)
            .map(|_| {
                corefoundation::collections::new_dictionary(
                    &[],
                    Policy::from_key_callbacks(),
                    Policy::from_type_callbacks(),
                    true,
                )
            })
            .collect();
        let array = corefoundation::collections::new_array(
            &dictionaries,
            Policy::from_type_callbacks(),
            false,
        );
        dictionaries.into_iter().for_each(crate::release);
        *attachments = array as usize;
    }
    *attachments as CFTypeRef
}

exports::exports! {
    functions {
        "CMSampleBufferCreate" => CMSampleBufferCreate,
        "CMSampleBufferGetTypeID" => CMSampleBufferGetTypeID,
        "CMSampleBufferIsValid" => CMSampleBufferIsValid,
        "CMSampleBufferDataIsReady" => CMSampleBufferDataIsReady,
        "CMSampleBufferGetNumSamples" => CMSampleBufferGetNumSamples,
        "CMSampleBufferGetDataBuffer" => CMSampleBufferGetDataBuffer,
        "CMSampleBufferGetFormatDescription" => CMSampleBufferGetFormatDescription,
        "CMSampleBufferGetImageBuffer" => CMSampleBufferGetImageBuffer,
        "CMSampleBufferGetPresentationTimeStamp" => CMSampleBufferGetPresentationTimeStamp,
        "CMSampleBufferGetSampleAttachmentsArray" => CMSampleBufferGetSampleAttachmentsArray,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use crate::block::{CMBlockBufferCreateContiguous, CMBlockBufferCreateWithMemoryBlock};
    use crate::time::CMTimeMake;

    fn retain_count(object: CFTypeRef) -> isize {
        // SAFETY: a live object.
        unsafe { corefoundation::api::CFGetRetainCount(object) }
    }

    fn create(
        data: CFTypeRef,
        samples: i64,
        timing: &[TimingInfo],
        sizes: &[usize],
    ) -> (i32, CFTypeRef) {
        let mut out = std::ptr::null();
        // SAFETY: arrays of the lengths passed and a live out-parameter.
        let status = unsafe {
            CMSampleBufferCreate(
                std::ptr::null(),
                data,
                1,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                samples,
                timing.len() as i64,
                timing.as_ptr(),
                sizes.len() as i64,
                sizes.as_ptr(),
                &mut out,
            )
        };
        (status, out)
    }

    /// Roblox's AppleVideoToolboxDecoder, step by step: borrow the access
    /// unit, copy it into a contiguous buffer, and wrap that in a one-sample
    /// buffer with the stream's description.
    #[test]
    fn roblox_cluster_a_sample_is_built_and_read_back() {
        let mut unit = vec![0u8, 0, 0, 3, 0x65, 0x88, 0x84];
        let mut description = std::ptr::null();
        // SAFETY: live arguments and out-parameters throughout; every object
        // is released at the end.
        unsafe {
            crate::CMVideoFormatDescriptionCreate(
                std::ptr::null(),
                crate::CODEC_H264,
                64,
                64,
                std::ptr::null(),
                &mut description,
            );
            let pts = CMTimeMake(33_333_333, 1_000_000_000);
            let duration = CMTimeMake(33_333_333, 1_000_000_000);
            let mut borrowed = std::ptr::null();
            assert_eq!(
                CMBlockBufferCreateWithMemoryBlock(
                    std::ptr::null(),
                    unit.as_mut_ptr().cast(),
                    unit.len(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    unit.len(),
                    0,
                    &mut borrowed,
                ),
                NO_ERR
            );
            let mut block = std::ptr::null();
            assert_eq!(
                CMBlockBufferCreateContiguous(
                    std::ptr::null(),
                    borrowed,
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    0,
                    crate::block::ALWAYS_COPY_DATA,
                    &mut block,
                ),
                NO_ERR
            );
            corefoundation::api::CFRelease(borrowed);
            let block_count = retain_count(block);
            let entry = TimingInfo {
                duration,
                presentation_time_stamp: pts,
                decode_time_stamp: crate::time::INVALID,
            };
            let size = unit.len();
            let mut sample = std::ptr::null();
            assert_eq!(
                CMSampleBufferCreate(
                    std::ptr::null(),
                    block,
                    1,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    description,
                    1,
                    1,
                    &entry,
                    1,
                    &size,
                    &mut sample,
                ),
                NO_ERR
            );
            assert_eq!(retain_count(block), block_count + 1);
            assert_eq!(
                corefoundation::api::CFGetTypeID(sample),
                CMSampleBufferGetTypeID()
            );
            assert_eq!(CMSampleBufferIsValid(sample), 1);
            assert_eq!(CMSampleBufferDataIsReady(sample), 1);
            assert_eq!(CMSampleBufferGetNumSamples(sample), 1);
            assert_eq!(CMSampleBufferGetDataBuffer(sample), block);
            assert_eq!(CMSampleBufferGetFormatDescription(sample), description);
            assert!(CMSampleBufferGetImageBuffer(sample).is_null());
            assert_eq!(CMSampleBufferGetPresentationTimeStamp(sample), pts);
            assert_eq!(timing(sample), (pts, duration));
            assert_eq!(crate::block::bytes(data(sample)), Some(&unit[..]));
            unit[4] = 0;
            assert_eq!(crate::block::bytes(block).unwrap()[4], 0x65, "a copy");
            assert_eq!(CMSampleBufferIsValid(block), 0);
            corefoundation::api::CFRelease(sample);
            assert_eq!(retain_count(block), block_count);
            corefoundation::api::CFRelease(block);
            corefoundation::api::CFRelease(description);
        }
    }

    #[test]
    fn entry_counts_are_checked() {
        let entry = TimingInfo {
            duration: crate::time::INVALID,
            presentation_time_stamp: CMTimeMake(1, 30),
            decode_time_stamp: crate::time::INVALID,
        };
        assert_eq!(
            create(std::ptr::null(), 3, &[entry; 2], &[]).0,
            INVALID_ENTRY_COUNT
        );
        assert_eq!(
            create(std::ptr::null(), 3, &[], &[1, 2]).0,
            INVALID_ENTRY_COUNT
        );
        assert_eq!(
            create(std::ptr::null(), -1, &[], &[]).0,
            INVALID_ENTRY_COUNT
        );
        for (timing, sizes) in [(0, 0), (1, 1), (3, 3), (1, 3)] {
            let (status, sample) =
                create(std::ptr::null(), 3, &vec![entry; timing], &vec![4; sizes]);
            assert_eq!(status, NO_ERR, "{timing} timing and {sizes} size entries");
            // SAFETY: a sample buffer this test made.
            unsafe { corefoundation::api::CFRelease(sample) };
        }
        let (_, sample) = create(std::ptr::null(), 0, &[], &[]);
        // SAFETY: as above.
        unsafe {
            assert_eq!(
                CMSampleBufferGetPresentationTimeStamp(sample),
                crate::time::INVALID
            );
            corefoundation::api::CFRelease(sample);
        }
    }

    #[test]
    fn make_data_ready_callback_is_refused() {
        let mut out = std::ptr::null();
        // SAFETY: a live out-parameter; the callback is never called.
        let status = unsafe {
            CMSampleBufferCreate(
                std::ptr::null(),
                std::ptr::null(),
                0,
                0x1000 as *const c_void,
                std::ptr::null_mut(),
                std::ptr::null(),
                0,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                &mut out,
            )
        };
        assert_eq!(status, REQUIRED_PARAMETER_MISSING);
        assert!(out.is_null());
    }

    #[test]
    fn attachments_array_is_made_only_on_request() {
        let (_, sample) = create(std::ptr::null(), 2, &[], &[]);
        // SAFETY: a sample buffer this test made, and the array it owns.
        unsafe {
            assert!(CMSampleBufferGetSampleAttachmentsArray(sample, 0).is_null());
            let array = CMSampleBufferGetSampleAttachmentsArray(sample, 1);
            assert!(!array.is_null());
            assert_eq!(CMSampleBufferGetSampleAttachmentsArray(sample, 0), array);
            assert_eq!(CMSampleBufferGetSampleAttachmentsArray(sample, 1), array);
            assert_eq!(corefoundation::api::CFArrayGetCount(array), 2);
            let first = corefoundation::api::CFArrayGetValueAtIndex(array, 0);
            let key = corefoundation::base::interned_name("NotSync") as usize as CFTypeRef;
            corefoundation::api::CFDictionarySetValue(
                first,
                key,
                corefoundation::collections::boolean(true),
            );
            assert_eq!(corefoundation::api::CFDictionaryGetCount(first), 1);
            let again = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
            assert_eq!(
                corefoundation::api::CFDictionaryGetCount(
                    corefoundation::api::CFArrayGetValueAtIndex(again, 0)
                ),
                1
            );
            corefoundation::api::CFRelease(sample);
        }
    }
}
