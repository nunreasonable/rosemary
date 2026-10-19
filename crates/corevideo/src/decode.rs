//! VideoToolbox's decoder and CoreMedia's format descriptions, as far as a
//! program probing for hardware video decoding reaches. There is no
//! hardware decoder behind this runtime, so a session is never made and a
//! program falls back to decoding in software, as it does on a Mac whose GPU
//! cannot decode the codec.

#![allow(non_snake_case)]

use std::ffi::c_void;

/// `kVTCouldNotFindVideoDecoderErr`.
const NO_DECODER: i32 = -12906;
/// `kCMFormatDescriptionError_InvalidParameter`.
const INVALID_PARAMETER: i32 = -12710;

/// `void VTRegisterSupplementalVideoDecoderIfAvailable(CMVideoCodecType)`.
pub unsafe extern "C" fn VTRegisterSupplementalVideoDecoderIfAvailable(_codec: u32) {}

/// `Boolean VTIsHardwareDecodeSupported(CMVideoCodecType)`: no.
pub unsafe extern "C" fn VTIsHardwareDecodeSupported(_codec: u32) -> u8 {
    0
}

/// `OSStatus VTDecompressionSessionCreate(...)`: no decoder.
///
/// # Safety
///
/// `out` is null or writable.
pub unsafe extern "C" fn VTDecompressionSessionCreate(
    _allocator: *const c_void,
    _format: *const c_void,
    _specification: *const c_void,
    _attributes: *const c_void,
    _callback: *const c_void,
    out: *mut *const c_void,
) -> i32 {
    if !out.is_null() {
        // SAFETY: writable, per the caller.
        unsafe { out.write(std::ptr::null()) };
    }
    NO_DECODER
}

/// `OSStatus VTCompressionSessionCreate(CFAllocatorRef, int32_t width, int32_t
/// height, CMVideoCodecType, CFDictionaryRef encoderSpecification,
/// CFDictionaryRef sourceImageBufferAttributes, CFAllocatorRef,
/// VTCompressionOutputCallback, void *, VTCompressionSessionRef *)`: no
/// encoder either, which sends a program to its software encoder.
///
/// # Safety
///
/// `out` is null or writable.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn VTCompressionSessionCreate(
    _allocator: *const c_void,
    _width: i32,
    _height: i32,
    _codec: u32,
    _specification: *const c_void,
    _attributes: *const c_void,
    _compressed_allocator: *const c_void,
    _callback: *const c_void,
    _refcon: *const c_void,
    out: *mut *const c_void,
) -> i32 {
    /// `kVTCouldNotFindVideoEncoderErr`.
    const NO_ENCODER: i32 = -12908;
    if !out.is_null() {
        // SAFETY: writable, per the caller.
        unsafe { out.write(std::ptr::null()) };
    }
    NO_ENCODER
}

/// `OSStatus CMVideoFormatDescriptionCreateFromH264ParameterSets(...)` and
/// the HEVC one: refused, which keeps a program off the decoder path above.
///
/// # Safety
///
/// `out` is null or writable.
pub unsafe extern "C" fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
    _allocator: *const c_void,
    _count: usize,
    _pointers: *const *const u8,
    _sizes: *const usize,
    _nal_length: i32,
    out: *mut *const c_void,
) -> i32 {
    if !out.is_null() {
        // SAFETY: writable, per the caller.
        unsafe { out.write(std::ptr::null()) };
    }
    INVALID_PARAMETER
}

/// `CGRect`.
#[repr(C)]
pub struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// `CGRect CMVideoFormatDescriptionGetCleanAperture(CMVideoFormatDescriptionRef,
/// Boolean)`: the empty rectangle, for a description that is never made here.
pub unsafe extern "C" fn CMVideoFormatDescriptionGetCleanAperture(
    _description: *const c_void,
    _origin_top_left: u8,
) -> Rect {
    Rect {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    }
}
