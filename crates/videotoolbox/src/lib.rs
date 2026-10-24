//! VideoToolbox: hardware and system video codecs behind decompression and
//! compression sessions.
//!
//! H.264 decompression sessions decode through the first backend that works
//! on this host (FFmpeg over Vulkan Video, VA-API or CUDA, loaded at run
//! time; see `backend`), and hand the callback pixel buffers in the format
//! the attributes ask for. Where no backend works, and for every other codec
//! and every compression session, create says so the way VideoToolbox does
//! on a machine without the codec, `kVTCouldNotFindVideoDecoderErr` or
//! `kVTCouldNotFindVideoEncoderErr`, which is the answer a program already
//! handles by decoding or encoding some other way. What a session would be
//! configured with, the specification and property keys, is published so
//! that building the request works.

#![allow(non_snake_case, clippy::missing_safety_doc)]

use std::ffi::c_void;

mod backend;
mod bitstream;
mod output;
mod session;

use corefoundation::base::CFTypeRef;

/// `OSStatus` values from `VTErrors.h`.
pub const PARAMETER_ERR: i32 = -12902;
pub const COULD_NOT_FIND_VIDEO_DECODER: i32 = -12906;
pub const COULD_NOT_FIND_VIDEO_ENCODER: i32 = -12908;
/// What every entry point below answers with: there is no codec, so a
/// `VTDecompressionSessionCreate`/`VTCompressionSessionCreate` call never hands
/// a caller a session, and every other entry point here only ever receives
/// `NULL` or a foreign pointer in its place.
pub const INVALID_SESSION_ERR: i32 = -12903;

/// The keys, as `(Mach-O name, text)`; the texts are Apple's.
const KEYS: &[(&str, &str)] = &[
    (
        "_kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder",
        "EnableHardwareAcceleratedVideoDecoder",
    ),
    (
        "_kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder",
        "RequireHardwareAcceleratedVideoDecoder",
    ),
    (
        "_kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder",
        "EnableHardwareAcceleratedVideoEncoder",
    ),
    (
        "_kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder",
        "RequireHardwareAcceleratedVideoEncoder",
    ),
    (
        "_kVTCompressionPropertyKey_AverageBitRate",
        "AverageBitRate",
    ),
    (
        "_kVTCompressionPropertyKey_DataRateLimits",
        "DataRateLimits",
    ),
    ("_kVTCompressionPropertyKey_RealTime", "RealTime"),
    (
        "_kVTCompressionPropertyKey_AllowFrameReordering",
        "AllowFrameReordering",
    ),
    (
        "_kVTCompressionPropertyKey_ExpectedFrameRate",
        "ExpectedFrameRate",
    ),
    (
        "_kVTCompressionPropertyKey_MaxKeyFrameInterval",
        "MaxKeyFrameInterval",
    ),
    (
        "_kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration",
        "MaxKeyFrameIntervalDuration",
    ),
    (
        "_kVTCompressionPropertyKey_MaxFrameDelayCount",
        "MaxFrameDelayCount",
    ),
    ("_kVTCompressionPropertyKey_ProfileLevel", "ProfileLevel"),
    (
        "_kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder",
        "UsingHardwareAcceleratedVideoEncoder",
    ),
    ("_kVTVideoEncoderList_CodecType", "CodecType"),
    ("_kVTVideoEncoderList_EncoderName", "EncoderName"),
    ("_kVTEncodeFrameOptionKey_ForceKeyFrame", "ForceKeyFrame"),
    (
        "_kVTProfileLevel_H264_Baseline_AutoLevel",
        "H264_Baseline_AutoLevel",
    ),
    (
        "_kVTProfileLevel_H264_Main_AutoLevel",
        "H264_Main_AutoLevel",
    ),
    (
        "_kVTProfileLevel_H264_High_AutoLevel",
        "H264_High_AutoLevel",
    ),
];

/// The data symbols this crate publishes.
pub fn data_symbols() -> Vec<(String, u64)> {
    corefoundation::bootstrap();
    let mut out = corefoundation::base::string_globals(KEYS);
    out.extend(corefoundation::base::string_globals(session::KEYS));
    out
}

/// `OSStatus VTDecompressionSessionCreate(CFAllocatorRef,
/// CMVideoFormatDescriptionRef, CFDictionaryRef videoDecoderSpecification,
/// CFDictionaryRef destinationImageBufferAttributes,
/// const VTDecompressionOutputCallbackRecord *, VTDecompressionSessionRef *)`.
pub unsafe extern "C" fn VTDecompressionSessionCreate(
    _allocator: CFTypeRef,
    format: CFTypeRef,
    specification: CFTypeRef,
    attributes: CFTypeRef,
    callback: *const session::CallbackRecord,
    out: *mut CFTypeRef,
) -> i32 {
    // SAFETY: null or an object a guest received from this runtime.
    let video = !format.is_null()
        && unsafe { coremedia::CMFormatDescriptionGetMediaType(format) } == coremedia::MEDIA_VIDEO;
    if out.is_null() || !video {
        return PARAMETER_ERR;
    }
    // SAFETY: the caller's arguments, `out` checked above.
    unsafe { session::create(format, specification, attributes, callback, out) }
}

/// `OSStatus VTCompressionSessionCreate(CFAllocatorRef, int32_t width,
/// int32_t height, CMVideoCodecType, CFDictionaryRef encoderSpecification,
/// CFDictionaryRef sourceImageBufferAttributes, CFAllocatorRef
/// compressedDataAllocator, VTCompressionOutputCallback, void *refcon,
/// VTCompressionSessionRef *)`.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn VTCompressionSessionCreate(
    _allocator: CFTypeRef,
    width: i32,
    height: i32,
    _codec: u32,
    _specification: CFTypeRef,
    _attributes: CFTypeRef,
    _compressed_allocator: CFTypeRef,
    _callback: *const std::ffi::c_void,
    _refcon: *mut std::ffi::c_void,
    out: *mut CFTypeRef,
) -> i32 {
    if out.is_null() || width <= 0 || height <= 0 {
        return PARAMETER_ERR;
    }
    // SAFETY: the caller's out-parameter, checked above.
    unsafe { out.write(std::ptr::null()) };
    COULD_NOT_FIND_VIDEO_ENCODER
}

/// `Boolean VTIsHardwareDecodeSupported(CMVideoCodecType)`: H.264 where a
/// hardware backend works.
pub extern "C" fn VTIsHardwareDecodeSupported(codec: u32) -> u8 {
    u8::from(backend::Registry::global().hardware_supports(codec))
}

/// `CVPixelBufferPoolRef VTCompressionSessionGetPixelBufferPool(
/// VTCompressionSessionRef)`: not retained, and never one since no session
/// exists to own it.
pub extern "C" fn VTCompressionSessionGetPixelBufferPool(_session: CFTypeRef) -> CFTypeRef {
    std::ptr::null()
}

/// `void VTCompressionSessionInvalidate(VTCompressionSessionRef)`: nothing to
/// invalidate.
pub extern "C" fn VTCompressionSessionInvalidate(_session: CFTypeRef) {}

/// `OSStatus VTCompressionSessionPrepareToEncodeFrames(VTCompressionSessionRef)`.
pub extern "C" fn VTCompressionSessionPrepareToEncodeFrames(_session: CFTypeRef) -> i32 {
    INVALID_SESSION_ERR
}

/// `OSStatus VTCompressionSessionCompleteFrames(VTCompressionSessionRef,
/// CMTime completeUntilPresentationTimeStamp)`.
///
/// `completeUntilPresentationTimeStamp` is read for nothing: the answer is the
/// same regardless of what time is named, because no session is ever
/// anything but null or foreign. It is still a real parameter here, typed as
/// `coremedia::Time` and not `CFTypeRef`, so a reader checking this signature
/// against `VTCompressionSession.h` finds the right shape - **not** wired
/// into `rosemary::abi::table`, for the reason given on
/// [`VTCompressionSessionEncodeFrame`].
pub extern "C" fn VTCompressionSessionCompleteFrames(
    _session: CFTypeRef,
    _complete_until_presentation_time_stamp: coremedia::Time,
) -> i32 {
    INVALID_SESSION_ERR
}

/// `OSStatus VTCompressionSessionEncodeFrame(VTCompressionSessionRef,
/// CVImageBufferRef, CMTime presentationTimeStamp, CMTime duration,
/// CFDictionaryRef frameProperties, void *sourceFrameRefcon,
/// VTEncodeInfoFlags *infoFlagsOut)`.
///
/// Two `CMTime`s by value - twenty-four bytes each, `MEMORY` class in SysV
/// AMD64 and so passed entirely on the stack, never in a register - the same
/// shape `crates/rosemary/src/shims/coremedia.rs`'s `encode_frame` already
/// bridges, but only for calls `rosemary` hands to the real, `macOS`-hosted
/// VideoToolbox: that shim is reached through `host_or_refuse`, which this
/// crate's own entry points never go through, and extending it to the
/// `exports!`-registered path `abi::table` describes would mean giving
/// `abi::table::SIGNATURES` a way to name a struct-by-value argument it
/// cannot name today (the same gap `AGGREGATE` documents for a *return*
/// struct). Rather than guess at that, this function is implemented and
/// exported - so a call that *does* reach it, from a native same-architecture
/// run where the guest's own call instruction needs no marshaller at all,
/// gets the real, honest answer - but is deliberately left out of
/// `abi::table::SIGNATURES`.
#[allow(clippy::too_many_arguments)]
pub extern "C" fn VTCompressionSessionEncodeFrame(
    _session: CFTypeRef,
    _image_buffer: CFTypeRef,
    _presentation_time_stamp: coremedia::Time,
    _duration: coremedia::Time,
    _frame_properties: CFTypeRef,
    _source_frame_refcon: *mut c_void,
    _info_flags_out: *mut u32,
) -> i32 {
    INVALID_SESSION_ERR
}

/// `OSStatus VTCopyVideoEncoderList(CFDictionaryRef, CFArrayRef *)`: no
/// encoder exists, so the list is always empty - `noErr`, with an array the
/// caller owns and must release.
pub unsafe extern "C" fn VTCopyVideoEncoderList(_options: CFTypeRef, out: *mut CFTypeRef) -> i32 {
    if out.is_null() {
        return PARAMETER_ERR;
    }
    // SAFETY: an empty array of `corefoundation`'s own callbacks, owned by the
    // caller from here.
    let list = unsafe {
        corefoundation::api::CFArrayCreate(
            std::ptr::null(),
            std::ptr::null(),
            0,
            &corefoundation::callbacks::TYPE_VALUE_CALLBACKS,
        )
    };
    // SAFETY: the caller's out-parameter, checked above.
    unsafe { out.write(list) };
    0
}

/// `void VTRegisterSupplementalVideoDecoderIfAvailable(CMVideoCodecType)`: no
/// supplemental decoder exists to register.
pub extern "C" fn VTRegisterSupplementalVideoDecoderIfAvailable(_codec_type: u32) {}

exports::exports! {
    functions {
        "VTDecompressionSessionCreate" => VTDecompressionSessionCreate,
        "VTCompressionSessionCreate" => VTCompressionSessionCreate,
        "VTIsHardwareDecodeSupported" => VTIsHardwareDecodeSupported,
        "VTCompressionSessionGetPixelBufferPool" => VTCompressionSessionGetPixelBufferPool,
        "VTCompressionSessionInvalidate" => VTCompressionSessionInvalidate,
        "VTCompressionSessionPrepareToEncodeFrames" =>
            VTCompressionSessionPrepareToEncodeFrames,
        "VTCompressionSessionCompleteFrames" => VTCompressionSessionCompleteFrames,
        "VTCompressionSessionEncodeFrame" => VTCompressionSessionEncodeFrame,
        "VTCopyVideoEncoderList" => VTCopyVideoEncoderList,
        "VTRegisterSupplementalVideoDecoderIfAvailable" =>
            VTRegisterSupplementalVideoDecoderIfAvailable,
    }
    groups { session }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decompression_session_finds_no_decoder_and_leaves_none_behind() {
        let mut format = std::ptr::null();
        let mut session: CFTypeRef = 1 as CFTypeRef;
        // SAFETY: live out-parameters.
        unsafe {
            coremedia::CMVideoFormatDescriptionCreate(
                std::ptr::null(),
                coremedia::CODEC_H264,
                640,
                480,
                std::ptr::null(),
                &mut format,
            );
            let status = VTDecompressionSessionCreate(
                std::ptr::null(),
                format,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                &mut session,
            );
            assert_eq!(status, COULD_NOT_FIND_VIDEO_DECODER);
            assert!(session.is_null());
            assert_eq!(
                VTDecompressionSessionCreate(
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    &mut session,
                ),
                PARAMETER_ERR,
                "a session needs a format"
            );
            corefoundation::api::CFRelease(format);
        }
        assert_eq!(VTIsHardwareDecodeSupported(u32::from_be_bytes(*b"mp4v")), 0);
        assert_eq!(VTIsHardwareDecodeSupported(u32::from_be_bytes(*b"hvc1")), 0);
        assert_eq!(
            VTIsHardwareDecodeSupported(coremedia::CODEC_H264),
            u8::from(backend::Registry::global().hardware_supports(coremedia::CODEC_H264))
        );
    }

    #[test]
    fn a_compression_session_finds_no_encoder() {
        let mut session: CFTypeRef = 1 as CFTypeRef;
        // SAFETY: a live out-parameter.
        let status = unsafe {
            VTCompressionSessionCreate(
                std::ptr::null(),
                1280,
                720,
                coremedia::CODEC_H264,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                &mut session,
            )
        };
        assert_eq!(status, COULD_NOT_FIND_VIDEO_ENCODER);
        assert!(session.is_null());
    }

    /// A foreign, non-null pointer: every one of these entry points only ever
    /// receives this or `NULL`, since nothing here ever hands out a session.
    const FOREIGN_SESSION: CFTypeRef = 0x1234 as CFTypeRef;

    #[test]
    fn a_compression_session_call_without_a_real_session_is_invalid() {
        assert!(VTCompressionSessionGetPixelBufferPool(FOREIGN_SESSION).is_null());
        assert_eq!(
            VTCompressionSessionPrepareToEncodeFrames(FOREIGN_SESSION),
            INVALID_SESSION_ERR
        );
        let time = || coremedia::Time {
            value: 0,
            timescale: 0,
            flags: 0,
            epoch: 0,
        };
        assert_eq!(
            VTCompressionSessionCompleteFrames(FOREIGN_SESSION, time()),
            INVALID_SESSION_ERR
        );
        let mut info_flags = 0u32;
        assert_eq!(
            VTCompressionSessionEncodeFrame(
                FOREIGN_SESSION,
                std::ptr::null(),
                time(),
                time(),
                std::ptr::null(),
                std::ptr::null_mut(),
                &mut info_flags,
            ),
            INVALID_SESSION_ERR
        );
        VTCompressionSessionInvalidate(FOREIGN_SESSION);
    }

    #[test]
    fn there_is_never_an_encoder_to_list() {
        let mut list: CFTypeRef = std::ptr::null();
        // SAFETY: a live out-parameter.
        unsafe {
            assert_eq!(VTCopyVideoEncoderList(std::ptr::null(), &mut list), 0);
            assert_eq!(
                VTCopyVideoEncoderList(std::ptr::null(), std::ptr::null_mut()),
                PARAMETER_ERR
            );
            assert!(!list.is_null());
            assert_eq!(corefoundation::api::CFArrayGetCount(list), 0);
            corefoundation::api::CFRelease(list);
        }
    }

    #[test]
    fn no_supplemental_decoder_registration_does_anything() {
        // Nothing to assert beyond "does not panic": there is no registry for
        // it to touch.
        VTRegisterSupplementalVideoDecoderIfAvailable(coremedia::CODEC_H264);
    }
}
