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

mod backend;
mod bitstream;
mod output;
mod session;

use corefoundation::base::CFTypeRef;

/// `OSStatus` values from `VTErrors.h`.
pub const PARAMETER_ERR: i32 = -12902;
pub const COULD_NOT_FIND_VIDEO_DECODER: i32 = -12906;
pub const COULD_NOT_FIND_VIDEO_ENCODER: i32 = -12908;

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

exports::exports! {
    functions {
        "VTDecompressionSessionCreate" => VTDecompressionSessionCreate,
        "VTCompressionSessionCreate" => VTCompressionSessionCreate,
        "VTIsHardwareDecodeSupported" => VTIsHardwareDecodeSupported,
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
}
